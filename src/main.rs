#[macro_use]
extern crate serde_derive;

mod xml;
use anyhow::{anyhow, bail, Result};
use dbus::{
    arg::{
        self,
        messageitem::{MessageItem, MessageItemArray, MessageItemDict},
        AppendAll, ArgType, IterAppend, ReadAll, RefArg,
    },
    message::MatchRule,
    nonblock::{
        stdintf::org_freedesktop_dbus::{Properties, PropertiesPropertiesChanged},
        MethodReply, MsgMatch, Proxy, SyncConnection,
    },
    strings, Message,
};
use futures::{
    channel::{mpsc::UnboundedReceiver, oneshot},
    future,
    prelude::*,
    select_biased,
};
use fxhash::FxHashMap;
use log::{error, warn};
use netidx::{
    chars::Chars,
    path::Path,
    pool::Pooled,
    publisher::{BindCfg, Publisher},
    subscriber::Value,
};
use netidx_protocols::rpc::server as rpc;
use netidx_tools_core::ClientParams;
use std::{
    boxed::Box,
    collections::{HashMap, HashSet},
    fmt::Display,
    iter,
    pin::Pin,
    str::FromStr,
    sync::Arc,
    time::Duration,
};
use structopt::StructOpt;
use tokio::task;

// make this an argument?
const TIMEOUT: Duration = Duration::from_secs(30);

#[derive(StructOpt, Debug)]
struct Params {
    #[structopt(flatten)]
    common: ClientParams,
    #[structopt(
        short = "b",
        long = "bind",
        help = "configure the bind address",
        default_value = "local"
    )]
    bind: BindCfg,
    #[structopt(
        long = "timeout",
        help = "require subscribers to consume values before timeout (seconds)"
    )]
    timeout: Option<u64>,
    #[structopt(
        long = "netidx-base",
        help = "the base path to publish under",
        default_value = "/local/dbus"
    )]
    netidx_base: Path,
}

async fn introspect(con: &Proxy<'_, Arc<SyncConnection>>) -> Result<xml::Node> {
    let (xml,): (String,) = con
        .method_call("org.freedesktop.DBus.Introspectable", "Introspect", ())
        .await?;
    Ok(xml::Node::from_reader(xml.as_bytes())?)
}

async fn list_names(con: &Proxy<'_, Arc<SyncConnection>>) -> Result<Vec<String>> {
    let (names,): (Vec<String>,) = con
        .method_call("org.freedesktop.DBus", "ListNames", ())
        .await?;
    Ok(names)
}

async fn list_activatable_names(con: &Proxy<'_, Arc<SyncConnection>>) -> Result<Vec<String>> {
    let (names,): (Vec<String>,) = con
        .method_call("org.freedesktop.DBus", "ListActivatableNames", ())
        .await?;
    Ok(names)
}

#[derive(Debug, Clone)]
struct NameOwnerChanged {
    name: String,
    old_owner: Option<String>,
    new_owner: Option<String>,
}

impl arg::ReadAll for NameOwnerChanged {
    fn read(i: &mut arg::Iter) -> Result<Self, arg::TypeMismatchError> {
        let name = i.read()?;
        let old_owner: String = i.read()?;
        let new_owner: String = i.read()?;
        let or_none = |s: String| if s.is_empty() { None } else { Some(s) };
        Ok(NameOwnerChanged {
            name,
            old_owner: or_none(old_owner),
            new_owner: or_none(new_owner),
        })
    }
}

fn dbus_value_to_netidx_value<V: RefArg>(v: &V) -> Value {
    match v.arg_type() {
        ArgType::Byte => Value::from(v.as_u64().unwrap() as u32),
        ArgType::Int16 => Value::from(v.as_i64().unwrap() as i32),
        ArgType::UInt16 => Value::from(v.as_u64().unwrap() as u32),
        ArgType::Int32 => Value::from(v.as_i64().unwrap() as i32),
        ArgType::UInt32 => Value::from(v.as_u64().unwrap() as u32),
        ArgType::Int64 => Value::from(v.as_i64().unwrap()),
        ArgType::UInt64 => Value::from(v.as_u64().unwrap()),
        ArgType::Double => Value::from(v.as_f64().unwrap()),
        ArgType::UnixFd => Value::from("<unix-fd>"),
        ArgType::Boolean => Value::from(v.as_i64().unwrap() == 1),
        ArgType::Invalid => Value::Error(Chars::from("invalid")),
        ArgType::String | ArgType::ObjectPath | ArgType::Signature => {
            Value::from(String::from(v.as_str().unwrap()))
        }
        ArgType::Variant => {
            let mut iter = v.as_iter().unwrap().map(|v| dbus_value_to_netidx_value(&v));
            match iter.next() {
                None => Value::Null,
                Some(v0) => match iter.next() {
                    None => v0,
                    Some(v1) => Value::from(
                        iter::once(v0)
                            .chain(iter::once(v1).chain(iter))
                            .collect::<Vec<_>>(),
                    ),
                },
            }
        }
        ArgType::Array | ArgType::DictEntry | ArgType::Struct => Value::from(
            v.as_iter()
                .unwrap()
                .map(|v| dbus_value_to_netidx_value(&v))
                .collect::<Vec<_>>(),
        ),
    }
}

fn netidx_value_to_dbus_value(v: &Value, typ: &DbusType) -> Result<MessageItem> {
    match typ {
        DbusType::Bool => Ok(MessageItem::Bool(v.clone().cast_to()?)),
        DbusType::Byte => Ok(MessageItem::Byte(v.clone().cast_to()?)),
        DbusType::Int16 => Ok(MessageItem::Int16(v.clone().cast_to()?)),
        DbusType::UInt16 => Ok(MessageItem::UInt16(v.clone().cast_to()?)),
        DbusType::Int32 => Ok(MessageItem::Int32(v.clone().cast_to()?)),
        DbusType::UInt32 => Ok(MessageItem::UInt32(v.clone().cast_to()?)),
        DbusType::Int64 => Ok(MessageItem::Int64(v.clone().cast_to()?)),
        DbusType::UInt64 => Ok(MessageItem::UInt64(v.clone().cast_to()?)),
        DbusType::Double => Ok(MessageItem::Double(v.clone().cast_to()?)),
        DbusType::Signature => Ok(MessageItem::Signature(
            strings::Signature::new(v.clone().cast_to::<String>()?)
                .map_err(|s| anyhow!("invalid signature {}", s))?,
        )),
        DbusType::ObjectPath => Ok(MessageItem::ObjectPath(
            strings::Path::new(v.clone().cast_to::<String>()?)
                .map_err(|e| anyhow!("invalid object path {}", e))?,
        )),
        DbusType::String => Ok(MessageItem::Str(v.clone().cast_to::<String>()?)),
        DbusType::UnixFd => bail!("can't send unix fds over netidx"),
        DbusType::Array(t) => {
            let elts = v
                .clone()
                .cast_to::<Vec<Value>>()?
                .into_iter()
                .map(|v| netidx_value_to_dbus_value(&v, &*t))
                .collect::<Result<Vec<MessageItem>>>()?;
            let sig = strings::Signature::new(typ.to_string())
                .map_err(|s| anyhow!("invalid array signature {}", s))?;
            Ok(MessageItem::Array(
                MessageItemArray::new(elts, sig).map_err(|e| anyhow!("{:?}", e))?,
            ))
        }
        DbusType::Dict { key, value } => {
            let elts = v
                .clone()
                .cast_to::<Vec<(Value, Value)>>()?
                .into_iter()
                .map(|(k, v)| {
                    let k = netidx_value_to_dbus_value(&k, &*key)?;
                    let v = netidx_value_to_dbus_value(&v, &*value)?;
                    Ok((k, v))
                })
                .collect::<Result<Vec<(MessageItem, MessageItem)>>>()?;
            let key = strings::Signature::new(key.to_string())
                .map_err(|e| anyhow!("invalid dict key signature {}", e))?;
            let value = strings::Signature::new(value.to_string())
                .map_err(|e| anyhow!("invalid dict value signature {}", e))?;
            Ok(MessageItem::Dict(
                MessageItemDict::new(elts, key, value).map_err(|e| anyhow!("{:?}", e))?,
            ))
        }
        DbusType::Struct(inner) => {
            let elts = v
                .clone()
                .cast_to::<Vec<Value>>()?
                .into_iter()
                .zip(inner.iter())
                .map(|(v, typ)| netidx_value_to_dbus_value(&v, typ))
                .collect::<Result<Vec<MessageItem>>>()?;
            let tl = inner.len();
            let el = elts.len();
            if el != tl {
                bail!("struct elements mismatch expected {} found {}", tl, el)
            }
            Ok(MessageItem::Struct(elts))
        }
        DbusType::Variant => match v {
            Value::I32(i) | Value::Z32(i) => {
                Ok(MessageItem::Variant(Box::new(MessageItem::Int32(*i))))
            }
            Value::U32(i) | Value::V32(i) => {
                Ok(MessageItem::Variant(Box::new(MessageItem::UInt32(*i))))
            }
            Value::I64(i) | Value::Z64(i) => {
                Ok(MessageItem::Variant(Box::new(MessageItem::Int64(*i))))
            }
            Value::U64(i) | Value::V64(i) => {
                Ok(MessageItem::Variant(Box::new(MessageItem::UInt64(*i))))
            }
            Value::F32(f) => Ok(MessageItem::Variant(Box::new(MessageItem::Double(
                (*f) as f64,
            )))),
            Value::F64(f) => Ok(MessageItem::Variant(Box::new(MessageItem::Double(*f)))),
            Value::True | Value::Ok => Ok(MessageItem::Variant(Box::new(MessageItem::Bool(true)))),
            Value::False | Value::Error(_) | Value::Null => {
                Ok(MessageItem::Variant(Box::new(MessageItem::Bool(false))))
            }
            Value::String(s) => Ok(MessageItem::Variant(Box::new(MessageItem::Str(
                s.to_string(),
            )))),
            Value::Bytes(_) => bail!("can't send raw bytes to dbus"),
            Value::Duration(_) | Value::DateTime(_) => Ok(MessageItem::Variant(Box::new(
                MessageItem::Str(v.to_string_naked()),
            ))),
            Value::Array(a) => {
                let elts = a
                    .iter()
                    .map(|v| netidx_value_to_dbus_value(v, &DbusType::Variant))
                    .collect::<Result<Vec<MessageItem>>>()?;
                let sig =
                    strings::Signature::new("av").map_err(|e| anyhow!("invalid sig {}", e))?;
                let a = MessageItemArray::new(elts, sig)
                    .map_err(|e| anyhow!("invalid array {:?}", e))?;
                Ok(MessageItem::Variant(Box::new(MessageItem::Array(a))))
            }
        },
    }
}

enum DbusType {
    Byte,
    Bool,
    Int16,
    UInt16,
    Int32,
    UInt32,
    Int64,
    UInt64,
    Double,
    UnixFd,
    String,
    ObjectPath,
    Signature,
    Variant,
    Array(Box<DbusType>),
    Struct(Vec<DbusType>),
    Dict {
        key: Box<DbusType>,
        value: Box<DbusType>,
    },
}

impl FromStr for DbusType {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        Self::from_bytes(s.as_bytes())
    }
}

impl Display for DbusType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Byte => write!(f, "y"),
            Self::Bool => write!(f, "b"),
            Self::Int16 => write!(f, "n"),
            Self::UInt16 => write!(f, "q"),
            Self::Int32 => write!(f, "i"),
            Self::UInt32 => write!(f, "u"),
            Self::Int64 => write!(f, "x"),
            Self::UInt64 => write!(f, "t"),
            Self::Double => write!(f, "d"),
            Self::String => write!(f, "s"),
            Self::ObjectPath => write!(f, "o"),
            Self::Signature => write!(f, "g"),
            Self::Array(inner) => write!(f, "a{}", &*inner),
            Self::Dict { key, value } => write!(f, "{{{}{}}}", &*key, &*value),
            Self::Struct(inner) => {
                write!(f, "(")?;
                for t in inner {
                    write!(f, "{}", t)?;
                }
                write!(f, ")")
            }
            Self::Variant => write!(f, "v"),
            Self::UnixFd => write!(f, "h"),
        }
    }
}

impl DbusType {
    fn from_bytes(b: &[u8]) -> Result<Self> {
        match b {
            [] => bail!("expected at least 1 character"),
            [b'y', ..] => Ok(Self::Byte),
            [b'b', ..] => Ok(Self::Bool),
            [b'n', ..] => Ok(Self::Int16),
            [b'q', ..] => Ok(Self::UInt16),
            [b'i', ..] => Ok(Self::Int32),
            [b'u', ..] => Ok(Self::UInt32),
            [b'x', ..] => Ok(Self::Int64),
            [b't', ..] => Ok(Self::UInt64),
            [b'd', ..] => Ok(Self::Double),
            [b's', ..] => Ok(Self::String),
            [b'o', ..] => Ok(Self::ObjectPath),
            [b'g', ..] => Ok(Self::Signature),
            [b'v', ..] => Ok(Self::Variant),
            [b'h', ..] => Ok(Self::UnixFd),
            [b'a', tail @ ..] => Ok(Self::Array(Box::new(Self::from_bytes(tail)?))),
            [b'{', s @ .., b'}'] => {
                if s.len() == 0 {
                    bail!("empty dict")
                }
                let mut s = s;
                let key = Box::new(Self::from_bytes(s)?);
                s = &s[key.len()..];
                let value = Box::new(Self::from_bytes(s)?);
                s = &s[value.len()..];
                if !s.is_empty() {
                    bail!("dict must contain exactly two types")
                }
                Ok(Self::Dict { key, value })
            }
            [b'(', s @ .., b')'] => {
                if s.len() == 0 {
                    bail!("empty struct type")
                }
                let mut elts = Vec::new();
                let mut s = s;
                while !s.is_empty() {
                    let elt = Self::from_bytes(s)?;
                    s = &s[elt.len()..];
                    elts.push(elt);
                }
                Ok(Self::Struct(elts))
            }
            _ => bail!("invalid dbus type"),
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::Byte
            | Self::Bool
            | Self::Int16
            | Self::UInt16
            | Self::Int32
            | Self::UInt32
            | Self::Int64
            | Self::UInt64
            | Self::Double
            | Self::String
            | Self::ObjectPath
            | Self::Signature
            | Self::UnixFd
            | Self::Variant => 1,
            Self::Array(elt) => 1 + Self::len(&elt),
            Self::Struct(elts) => 2 + elts.iter().map(Self::len).sum::<usize>(),
            Self::Dict { key, value } => 2 + Self::len(key) + Self::len(value),
        }
    }
}

#[derive(Clone, Copy)]
enum DbusArgDirection {
    In,
    Out,
}

struct DbusMethodArgSpec {
    name: Option<String>,
    typ: DbusType,
    direction: DbusArgDirection,
}

impl<'a> TryFrom<&'a xml::Arg> for DbusMethodArgSpec {
    type Error = anyhow::Error;

    fn try_from(value: &'a xml::Arg) -> Result<Self> {
        Ok(Self {
            name: value.name.clone(),
            typ: DbusType::from_str(&value.typ)?,
            direction: match &value.direction {
                None => DbusArgDirection::In,
                Some(ref t) if t == "in" => DbusArgDirection::In,
                Some(ref t) if t == "out" => DbusArgDirection::Out,
                Some(d) => bail!("invalid arg direction {}", d),
            },
        })
    }
}

struct DbusMethodArgs(Vec<MessageItem>);

impl AppendAll for DbusMethodArgs {
    fn append(&self, i: &mut IterAppend) {
        for v in &self.0 {
            v.append(i);
        }
    }
}

impl DbusMethodArgs {
    fn new<'a>(
        sig: &Vec<DbusMethodArgSpec>,
        vals: &mut HashMap<Arc<str>, Pooled<Vec<Value>>>,
    ) -> Result<Self> {
        let elts = sig
            .iter()
            .map(|a| {
                let v = vals
                    .remove(a.name.as_ref().unwrap().as_str())
                    .ok_or_else(|| anyhow!("missing argument"))?
                    .pop()
                    .ok_or_else(|| anyhow!("empty argument"))?;
                netidx_value_to_dbus_value(&v, &a.typ)
            })
            .collect::<Result<Vec<_>>>()?;
        let sl = sig.len();
        let el = elts.len();
        if sl != el {
            bail!("arity mismatch, expected {} received {}", sl, el)
        }
        Ok(Self(elts))
    }
}

struct DbusMethodRet(Value);

impl ReadAll for DbusMethodRet {
    fn read(i: &mut arg::Iter) -> Result<Self, arg::TypeMismatchError> {
        let mut elts = Vec::new();
        loop {
            match i.get_refarg() {
                None => break,
                Some(a) => {
                    elts.push(dbus_value_to_netidx_value(&a));
                    if !i.next() {
                        break;
                    }
                }
            }
        }
        if elts.len() == 0 {
            Ok(Self(Value::Null))
        } else if elts.len() == 1 {
            Ok(Self(elts.pop().unwrap()))
        } else {
            Ok(Self(Value::from(elts)))
        }
    }
}

struct ProxiedMethod(rpc::Proc);

impl ProxiedMethod {
    fn new(
        base: Path,
        publisher: &Publisher,
        proxy: Proxy<'static, Arc<SyncConnection>>,
        interface: String,
        method: xml::Method,
    ) -> Result<Self> {
        let (mut arg_spec, ret_spec): (Vec<DbusMethodArgSpec>, Vec<DbusMethodArgSpec>) = method
            .args()
            .into_iter()
            .map(DbusMethodArgSpec::try_from)
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .partition(|a| match a.direction {
                DbusArgDirection::In => true,
                DbusArgDirection::Out => false,
            });
        {
            let mut uargs = HashSet::new();
            let mut nargs = 0;
            for a in &mut arg_spec {
                loop {
                    let n = match &a.name {
                        Some(n) => n.clone(),
                        None => {
                            let n = format!("anon{}", nargs);
                            a.name = Some(n.clone());
                            nargs += 1;
                            n
                        }
                    };
                    if uargs.contains(&n) {
                        a.name.as_mut().unwrap().push('_');
                    } else {
                        uargs.insert(n);
                        break;
                    }
                }
            }
        }
        struct Spec {
            arg_spec: Vec<DbusMethodArgSpec>,
            ret_spec: Vec<DbusMethodArgSpec>,
            interface: String,
            method: String,
            proxy: Proxy<'static, Arc<SyncConnection>>,
        }
        let base = base.append(&method.name);
        let spec = Arc::new(Spec {
            arg_spec,
            ret_spec,
            interface,
            method: method.name,
            proxy,
        });
        let desc = {
            use std::fmt::Write;
            let mut desc = String::with_capacity(32);
            let s = "proxied dbus method";
            desc.push_str(s);
            for a in &spec.ret_spec {
                if desc.len() == s.len() {
                    desc.push_str(" return typ: ");
                }
                let _ = write!(desc, "{}", a.typ);
            }
            desc
        };
        let proc = rpc::Proc::new(
            publisher,
            base,
            Value::from(desc),
            spec.arg_spec
                .iter()
                .map(|a| {
                    let name = Arc::from(a.name.as_ref().unwrap().as_str());
                    let spec = (Value::Null, Value::from(a.typ.to_string()));
                    (name, spec)
                })
                .collect(),
            Arc::new(move |_clid, mut args| {
                let spec = Arc::clone(&spec);
                Box::pin(async move {
                    match DbusMethodArgs::new(&spec.arg_spec, &mut *args) {
                        Err(e) => Value::Error(Chars::from(format!(
                            "failed to construct dbus args: {}",
                            e
                        ))),
                        Ok(dargs) => {
                            if !args.is_empty() {
                                warn!("ignoring extra args in method call")
                            }
                            let r: MethodReply<DbusMethodRet> =
                                spec.proxy.method_call(&spec.interface, &spec.method, dargs);
                            match r.await {
                                Err(e) => {
                                    Value::Error(Chars::from(format!("method call failed: {}", e)))
                                }
                                Ok(r) => r.0,
                            }
                        }
                    }
                })
            }),
        )?;
        Ok(Self(proc))
    }
}

struct Object {
    _methods: Vec<ProxiedMethod>,
    _children: Vec<Object>,
}

impl Object {
    fn publish_methods(
        base: &Path,
        publisher: &Publisher,
        proxy: &Proxy<'static, Arc<SyncConnection>>,
        node: &xml::Node,
    ) -> Vec<ProxiedMethod> {
        node.interfaces()
            .into_iter()
            .flat_map(|i| {
                i.methods().into_iter().filter_map(|m| {
                    let base = base.append("interfaces").append(&i.name).append("methods");
                    match ProxiedMethod::new(
                        base.clone(),
                        publisher,
                        proxy.clone(),
                        i.name.clone(),
                        m.clone(),
                    ) {
                        Ok(p) => Some(p),
                        Err(e) => {
                            warn!("failed to proxy method {} {}", base, e);
                            None
                        }
                    }
                })
            })
            .collect()
    }

    async fn publish_properties(
        base: Path,
        publisher: Publisher,
        proxy: Proxy<'_, Arc<SyncConnection>>,
        node: xml::Node,
        mut stop: future::Shared<oneshot::Receiver<()>>,
    ) -> Result<()> {
        let (filter, mut changes): (
            MsgMatch,
            UnboundedReceiver<(Message, PropertiesPropertiesChanged)>,
        ) = proxy
            .connection
            .add_match(
                MatchRule::new()
                    .with_sender(proxy.destination.clone().into_static())
                    .with_path(proxy.path.clone().into_static())
                    .with_interface("org.freedesktop.DBus.Properties")
                    .with_member("PropertiesChanged"),
            )
            .await?
            .stream();
        let cleanup = {
            let connection = proxy.connection.clone();
            || async move {
                let _: std::result::Result<_, _> = connection.remove_match(filter.token()).await;
            }
        };
        let mut properties = future::join_all(node.interfaces().into_iter().map(|i| {
            let proxy = &proxy;
            let publisher = &publisher;
            let base = &base;
            async move {
                let i = i.name.clone();
                let props = proxy
                    .get_all(&i)
                    .await?
                    .into_iter()
                    .map(|(name, value)| {
                        let path = base
                            .append("interfaces")
                            .append(&i)
                            .append("properties")
                            .append(&name);
                        let val = publisher.publish(path, dbus_value_to_netidx_value(&value))?;
                        Ok((name, val))
                    })
                    .collect::<Result<HashMap<_, _>>>()?;
                Ok::<_, anyhow::Error>((i, props))
            }
        }))
        .await
        .into_iter()
        .filter_map(|r| match r {
            Ok(vals) => Some(vals),
            Err(e) => {
                warn!("couldn't proxy properties for interface {}", e);
                None
            }
        })
        .collect::<FxHashMap<_, _>>();
        loop {
            let mut batch = publisher.start_batch();
            select_biased! {
                (_, change) = changes.select_next_some() => {
                    if let Some(intf) = properties.get_mut(&change.interface_name) {
                        for inv in &change.invalidated_properties {
                            intf.remove(inv);
                        }
                        for (name, value) in change.changed_properties {
                            match intf.get(&name) {
                                Some(val) => val.update(&mut batch, dbus_value_to_netidx_value(&value)),
                                None => {
                                    let path = base.append("interfaces").append(&change.interface_name).append(&name);
                                    let val = publisher.publish(path, dbus_value_to_netidx_value(&value))?;
                                    intf.insert(name, val);
                                }
                            }
                        }
                        if intf.len() == 0 {
                            properties.remove(&change.interface_name);
                        }
                    }
                }
                _ = stop => {
                    cleanup().await;
                    break
                },
                complete => {
                    cleanup().await;
                    break
                },
            }
            batch.commit(None).await
        }
        Ok(())
    }

    fn new(
        base: Path,
        publisher: Publisher,
        proxy: Proxy<'static, Arc<SyncConnection>>,
        stop: future::Shared<oneshot::Receiver<()>>,
    ) -> Pin<Box<dyn Future<Output = Result<Object>>>> {
        Box::into_pin(Box::new(async move {
            let node = introspect(&proxy).await?;
            if node
                .interfaces()
                .iter()
                .any(|i| i.name.as_str() == "org.freedesktop.DBus.Properties")
            {
                let base = base.clone();
                let publisher = publisher.clone();
                let proxy = proxy.clone();
                let node = node.clone();
                let stop = stop.clone();
                task::spawn(async move {
                    let path = proxy.path.clone();
                    let dest = proxy.destination.clone();
                    match Self::publish_properties(base, publisher, proxy, node, stop).await {
                        Ok(()) => warn!("properties publisher for {}:{} stopped", dest, path),
                        Err(e) => warn!("properties publisher for {}:{} failed {}", dest, path, e),
                    }
                });
            }
            let _methods = Self::publish_methods(&base, &publisher, &proxy, &node);
            let _children = future::join_all(
                node.nodes()
                    .into_iter()
                    .map(|c| {
                        let base = c
                            .name
                            .as_ref()
                            .map(|n| base.append(n))
                            .unwrap_or_else(|| base.clone());
                        let path = strings::Path::new(
                            c.name
                                .as_ref()
                                .map(|n| {
                                    if &*proxy.path == "/" {
                                        format!("/{}", n)
                                    } else {
                                        format!("{}/{}", proxy.path, n)
                                    }
                                })
                                .unwrap_or_else(|| String::from(&*proxy.path)),
                        )
                        .map_err(|_| anyhow!("invalid path {}", base))?;
                        let proxy = Proxy::new(
                            proxy.destination.clone(),
                            path,
                            TIMEOUT,
                            Arc::clone(&proxy.connection),
                        );
                        Ok::<_, anyhow::Error>(Self::new(
                            base,
                            publisher.clone(),
                            proxy,
                            stop.clone(),
                        ))
                    })
                    .filter_map(|r| match r {
                        Ok(f) => Some(f),
                        Err(e) => {
                            warn!("failed to proxy child {}", e);
                            None
                        }
                    }),
            )
            .await
            .into_iter()
            .filter_map(|r| match r {
                Ok(o) => Some(o),
                Err(e) => {
                    warn!("failed to proxy child {}", e);
                    None
                }
            })
            .collect::<Vec<_>>();
            Ok(Object {
                _methods,
                _children,
            })
        }))
    }
}

struct ProxiedBusName {
    _root: Object,
    _stop: oneshot::Sender<()>,
}

impl ProxiedBusName {
    async fn new(
        con: &Arc<SyncConnection>,
        publisher: Publisher,
        base: Path,
        name: String,
    ) -> Result<Self> {
        let (_stop, receiver) = oneshot::channel();
        let proxy = Proxy::new(name, "/", TIMEOUT, con.clone());
        let _root = Object::new(base, publisher, proxy, receiver.shared()).await?;
        Ok(ProxiedBusName { _root, _stop })
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    let opts = Params::from_args();
    let (cfg, auth) = opts.common.load();
    let (dbus, con) = dbus_tokio::connection::new_session_sync()?;
    task::spawn(async move {
        let res = dbus.await;
        error!("lost connection to dbus {}", res);
    });
    let publisher = Publisher::new(cfg, auth, opts.bind).await?;
    let base = opts.netidx_base.clone();
    let dbus = Proxy::new("org.freedesktop.DBus", "/", TIMEOUT, Arc::clone(&con));
    let dbus_signal_match = con
        .add_match(
            MatchRule::new()
                .with_sender("org.freedesktop.DBus")
                .with_path("/")
                .with_type(dbus::MessageType::Signal),
        )
        .await?;
    let token = dbus_signal_match.token();
    let (dbus_signal_match, mut signals) = dbus_signal_match.msg_stream();
    /* I need to work out how to deal with activatable names
    let names = list_activatable_names(&dbus)
        .await?
        .into_iter()
        .chain(list_names(&dbus).await?.into_iter())
        .filter(|n| !n.starts_with(":"))
        .collect::<HashSet<_>>();
    */
    let names = list_names(&dbus)
        .await?
        .into_iter()
        .filter(|n| !n.starts_with(":"))
        .collect::<HashSet<_>>();
    let start_proxying = |name: String| {
        let base = base.append(&name);
        let con = &con;
        let publisher = publisher.clone();
        async move {
            let r = ProxiedBusName::new(con, publisher, base, name.clone()).await;
            match r {
                Ok(o) => Some(o),
                Err(e) => {
                    warn!("failed to proxy bus name {}: {}", name, e);
                    None
                }
            }
        }
    };
    let mut names = future::join_all(
        names
            .into_iter()
            .map(|n| async { (n.clone(), start_proxying(n).await) }),
    )
    .await
    .into_iter()
    .filter_map(|(name, r)| r.map(move |r| (name, r)))
    .collect::<FxHashMap<_, _>>();
    while let Some(msg) = signals.next().await {
        match msg.member() {
            None => (),
            Some(m) if &*m == "NameOwnerChanged" => {
                if let Ok(up) = msg.read_all::<NameOwnerChanged>() {
                    if up.new_owner.is_none() {
                        names.remove(up.name.as_str());
                    } else if up.old_owner.is_none() && !up.name.starts_with(":") {
                        if let Some(o) = start_proxying(up.name.clone()).await {
                            names.insert(up.name, o);
                        }
                    }
                }
            }
            /* I need to work out how to deal with activatable names
            Some(m) if &*m == "ActivatableServicesChanged" => {
                for name in list_activatable_names(&dbus).await? {
                    if !names.contains_key(&name) {
                        if let Some(o) = start_proxying(name.clone()).await {
                            names.insert(name, o);
                        }
                    }
                }
            }
             */
            Some(_) => (),
        }
    }
    dbus.connection.remove_match(token).await?;
    drop(dbus_signal_match);
    Ok(())
}
