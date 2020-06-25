pub use crate::resolver_single::Auth;
use crate::{
    config::Config,
    path::Path,
    pool::{Pool, Pooled},
    protocol::resolver::v1::{FromRead, FromWrite, Referral, Resolved, ToRead, ToWrite},
    resolver_single::{
        ResolverRead as SingleRead, ResolverWrite as SingleWrite, RAWFROMREADPOOL,
        RAWFROMWRITEPOOL,
    },
};
use anyhow::Result;
use futures::future;
use fxhash::FxBuildHasher;
use parking_lot::{Mutex, RwLock};
use std::{
    collections::{
        BTreeMap,
        Bound::{self, Included, Unbounded},
        HashMap,
    },
    iter::IntoIterator,
    marker::PhantomData,
    mem,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    ops::{Deref, DerefMut},
    result,
    sync::Arc,
    time::Duration,
};
use tokio::{sync::oneshot, time::Instant};

const MAX_REFERRALS: usize = 128;

trait ToPath {
    fn path(&self) -> Option<&Path>;
}

impl ToPath for ToRead {
    fn path(&self) -> Option<&Path> {
        match self {
            ToRead::List(p) | ToRead::Table(p) | ToRead::Resolve(p) => Some(p),
        }
    }
}

impl ToPath for ToWrite {
    fn path(&self) -> Option<&Path> {
        match self {
            ToWrite::Clear | ToWrite::Heartbeat => None,
            ToWrite::Publish(p) | ToWrite::Unpublish(p) | ToWrite::PublishDefault(p) => {
                Some(p)
            }
        }
    }
}

#[derive(Debug)]
struct Router {
    cached: BTreeMap<Path, (Instant, Referral)>,
}

impl Router {
    fn new() -> Self {
        Router { cached: BTreeMap::new() }
    }

    fn route_batch<T>(
        &mut self,
        pool: &Pool<Vec<(usize, T)>>,
        batch: &Pooled<Vec<T>>,
    ) -> impl Iterator<Item = (Option<Path>, Pooled<Vec<(usize, T)>>)>
    where
        T: ToPath + Clone + Send + Sync + 'static,
    {
        let now = Instant::now();
        let mut batches = HashMap::new();
        let mut gc = Vec::new();
        let mut id = 0;
        for v in batch.iter() {
            let v = v.clone();
            match v.path() {
                None => batches.entry(None).or_insert_with(|| pool.take()).push((id, v)),
                Some(path) => {
                    let mut r = self.cached.range::<str, (Bound<&str>, Bound<&str>)>((
                        Unbounded,
                        Included(&*path),
                    ));
                    loop {
                        match r.next_back() {
                            None => {
                                batches
                                    .entry(None)
                                    .or_insert_with(|| pool.take())
                                    .push((id, v));
                                break;
                            }
                            Some((p, (exp, _))) => {
                                if !path.starts_with(p.as_ref()) {
                                    continue;
                                } else {
                                    if &now < exp {
                                        batches
                                            .entry(Some(p.clone()))
                                            .or_insert_with(|| pool.take())
                                            .push((id, v))
                                    } else {
                                        gc.push(p.clone());
                                        batches
                                            .entry(None)
                                            .or_insert_with(|| pool.take())
                                            .push((id, v))
                                    }
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            id += 1;
        }
        for p in gc {
            self.cached.remove(p.as_ref());
        }
        batches.into_iter().map(|(p, batch)| match p {
            None => (None, batch),
            Some(p) => (Some(p), batch),
        })
    }

    fn get_referral(&self, path: &Path) -> Option<&Referral> {
        self.cached.get(path.as_ref()).map(|(_, r)| r)
    }

    fn add_referral(&mut self, r: Referral) {
        let exp = Instant::now() + Duration::from_secs(r.ttl);
        self.cached.insert(r.path.clone(), (exp, r));
    }
}

trait ToReferral: Sized {
    fn referral(self) -> result::Result<Referral, Self>;
}

impl ToReferral for FromRead {
    fn referral(self) -> result::Result<Referral, Self> {
        match self {
            FromRead::Referral(r) => Ok(r),
            m => Err(m),
        }
    }
}

impl ToReferral for FromWrite {
    fn referral(self) -> result::Result<Referral, Self> {
        match self {
            FromWrite::Referral(r) => Ok(r),
            m => Err(m),
        }
    }
}

trait Connection<T, F>
where
    T: ToPath,
    F: ToReferral,
{
    fn new(
        resolver: Config,
        desired_auth: Auth,
        writer_addr: SocketAddr,
        secrets: Arc<RwLock<HashMap<SocketAddr, u128, FxBuildHasher>>>,
    ) -> Self;
    fn send(
        &mut self,
        batch: Pooled<Vec<(usize, T)>>,
    ) -> oneshot::Receiver<Pooled<Vec<(usize, F)>>>;
}

impl Connection<ToRead, FromRead> for SingleRead {
    fn new(
        resolver: Config,
        desired_auth: Auth,
        _writer_addr: SocketAddr,
        _secrets: Arc<RwLock<HashMap<SocketAddr, u128, FxBuildHasher>>>,
    ) -> Self {
        SingleRead::new(resolver, desired_auth)
    }

    fn send(
        &mut self,
        batch: Pooled<Vec<(usize, ToRead)>>,
    ) -> oneshot::Receiver<Pooled<Vec<(usize, FromRead)>>> {
        SingleRead::send(self, batch)
    }
}

impl Connection<ToWrite, FromWrite> for SingleWrite {
    fn new(
        resolver: Config,
        desired_auth: Auth,
        writer_addr: SocketAddr,
        secrets: Arc<RwLock<HashMap<SocketAddr, u128, FxBuildHasher>>>,
    ) -> Self {
        SingleWrite::new(resolver, desired_auth, writer_addr, secrets)
    }

    fn send(
        &mut self,
        batch: Pooled<Vec<(usize, ToWrite)>>,
    ) -> oneshot::Receiver<Pooled<Vec<(usize, FromWrite)>>> {
        SingleWrite::send(self, batch)
    }
}

lazy_static! {
    static ref RAWTOREADPOOL: Pool<Vec<ToRead>> = Pool::new(1000);
    static ref TOREADPOOL: Pool<Vec<(usize, ToRead)>> = Pool::new(1000);
    static ref FROMREADPOOL: Pool<Vec<(usize, FromRead)>> = Pool::new(1000);
    static ref RAWTOWRITEPOOL: Pool<Vec<ToWrite>> = Pool::new(1000);
    static ref TOWRITEPOOL: Pool<Vec<(usize, ToWrite)>> = Pool::new(1000);
    static ref FROMWRITEPOOL: Pool<Vec<(usize, FromWrite)>> = Pool::new(1000);
    static ref RESOLVEDPOOL: Pool<Vec<Resolved>> = Pool::new(1000);
}

#[derive(Debug)]
struct ResolverWrapInner<C, T, F> {
    router: Router,
    desired_auth: Auth,
    default: C,
    by_path: HashMap<Path, C>,
    writer_addr: SocketAddr,
    secrets: Arc<RwLock<HashMap<SocketAddr, u128, FxBuildHasher>>>,
    phantom: PhantomData<(T, F)>,
    f_pool: Pool<Vec<F>>,
    fi_pool: Pool<Vec<(usize, F)>>,
    ti_pool: Pool<Vec<(usize, T)>>,
}

#[derive(Debug, Clone)]
struct ResolverWrap<C, T, F>(Arc<Mutex<ResolverWrapInner<C, T, F>>>);

impl<C, T, F> ResolverWrap<C, T, F>
where
    C: Connection<T, F> + Clone + 'static,
    T: ToPath + Clone + Send + Sync + 'static,
    F: ToReferral + Clone + Send + Sync + 'static,
{
    fn new(
        default: Config,
        desired_auth: Auth,
        writer_addr: SocketAddr,
        f_pool: Pool<Vec<F>>,
        fi_pool: Pool<Vec<(usize, F)>>,
        ti_pool: Pool<Vec<(usize, T)>>,
    ) -> ResolverWrap<C, T, F> {
        let secrets =
            Arc::new(RwLock::new(HashMap::with_hasher(FxBuildHasher::default())));
        let router = Router::new();
        let default = C::new(default, desired_auth.clone(), writer_addr, secrets.clone());
        ResolverWrap(Arc::new(Mutex::new(ResolverWrapInner {
            router,
            desired_auth,
            default,
            by_path: HashMap::new(),
            writer_addr,
            secrets,
            f_pool,
            fi_pool,
            ti_pool,
            phantom: PhantomData,
        })))
    }

    fn secrets(&self) -> Arc<RwLock<HashMap<SocketAddr, u128, FxBuildHasher>>> {
        self.0.lock().secrets.clone()
    }

    async fn send(&self, batch: &Pooled<Vec<T>>) -> Result<Pooled<Vec<F>>> {
        let mut referrals = 0;
        loop {
            let mut waiters = Vec::new();
            let (mut finished, mut res) = {
                let mut guard = self.0.lock();
                let inner = &mut *guard;
                if inner.by_path.len() > MAX_REFERRALS {
                    inner.by_path.clear(); // a workable sledgehammer
                }
                for (r, batch) in inner.router.route_batch(&inner.ti_pool, batch) {
                    match r {
                        None => waiters.push(inner.default.send(batch)),
                        Some(rp) => match inner.by_path.get_mut(&rp) {
                            Some(con) => waiters.push(con.send(batch)),
                            None => {
                                let r = inner.router.get_referral(&rp).unwrap().clone();
                                let mut con = C::new(
                                    Config::from(r),
                                    inner.desired_auth.clone(),
                                    inner.writer_addr,
                                    inner.secrets.clone(),
                                );
                                inner.by_path.insert(rp, con.clone());
                                waiters.push(con.send(batch))
                            }
                        },
                    }
                }
                (inner.fi_pool.take(), inner.f_pool.take())
            };
            let qresult = future::join_all(waiters).await;
            let mut referral = false;
            for r in qresult {
                let mut r = r?;
                for (id, reply) in r.drain(..) {
                    match reply.referral() {
                        Ok(r) => {
                            self.0.lock().router.add_referral(r);
                            referral = true;
                        }
                        Err(m) => finished.push((id, m)),
                    }
                }
            }
            if !referral {
                finished.sort_by_key(|(id, _)| *id);
                res.extend(finished.drain(..).map(|(_, m)| m));
                break Ok(res);
            }
            referrals += 1;
            if referrals > MAX_REFERRALS {
                bail!("maximum referral depth {} reached, giving up", MAX_REFERRALS);
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResolverRead(ResolverWrap<SingleRead, ToRead, FromRead>);

impl ResolverRead {
    pub fn new(default: Config, desired_auth: Auth) -> Self {
        ResolverRead(ResolverWrap::new(
            default,
            desired_auth,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 0),
            RAWFROMREADPOOL.clone(),
            FROMREADPOOL.clone(),
            TOREADPOOL.clone(),
        ))
    }

    pub async fn send(
        &self,
        batch: &Pooled<Vec<ToRead>>,
    ) -> Result<Pooled<Vec<FromRead>>> {
        self.0.send(batch).await
    }

    pub async fn resolve<I>(&self, batch: I) -> Result<Pooled<Vec<Resolved>>>
    where
        I: IntoIterator<Item = Path>,
    {
        let mut to = RAWTOREADPOOL.take();
        to.extend(batch.into_iter().map(ToRead::Resolve));
        let mut result = self.send(&to).await?;
        if result.len() != to.len() {
            bail!(
                "unexpected number of resolve results {} expected {}",
                result.len(),
                to.len()
            )
        } else {
            let mut out = RESOLVEDPOOL.take();
            for r in result.drain(..) {
                match r {
                    FromRead::Resolved(r) => {
                        out.push(r);
                    }
                    m => bail!("unexpected resolve response {:?}", m),
                }
            }
            Ok(out)
        }
    }

    pub async fn list(&self, path: Path) -> Result<Pooled<Vec<Path>>> {
        let mut to = RAWTOREADPOOL.take();
        to.push(ToRead::List(path));
        let mut result = self.send(&to).await?;
        if result.len() != 1 {
            bail!("expected 1 result from list got {}", result.len());
        } else {
            match result.pop().unwrap() {
                FromRead::List(paths) => Ok(paths),
                m => bail!("unexpected result from list {:?}", m),
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResolverWrite(ResolverWrap<SingleWrite, ToWrite, FromWrite>);

impl ResolverWrite {
    pub fn new(default: Config, desired_auth: Auth, writer_addr: SocketAddr) -> Self {
        ResolverWrite(ResolverWrap::new(
            default,
            desired_auth,
            writer_addr,
            RAWFROMWRITEPOOL.clone(),
            FROMWRITEPOOL.clone(),
            TOWRITEPOOL.clone(),
        ))
    }

    pub async fn send(
        &self,
        batch: &Pooled<Vec<ToWrite>>,
    ) -> Result<Pooled<Vec<FromWrite>>> {
        self.0.send(batch).await
    }

    async fn send_expect<F, I>(&self, batch: I, f: F, expected: FromWrite) -> Result<()>
    where
        F: Fn(Path) -> ToWrite,
        I: IntoIterator<Item = Path>,
    {
        let mut to = RAWTOWRITEPOOL.take();
        let len = to.len();
        to.extend(batch.into_iter().map(f));
        let mut from = self.0.send(&to).await?;
        if from.len() != to.len() {
            bail!("unexpected number of responses {} vs expected {}", from.len(), len);
        }
        for (i, reply) in from.drain(..).enumerate() {
            if reply != expected {
                bail!("unexpected response to {:?}, {:?}", &to[i], reply)
            }
        }
        Ok(())
    }

    pub async fn publish<I: IntoIterator<Item = Path>>(&self, batch: I) -> Result<()> {
        self.send_expect(batch, ToWrite::Publish, FromWrite::Published).await
    }

    pub async fn publish_default<I: IntoIterator<Item = Path>>(
        &self,
        batch: I,
    ) -> Result<()> {
        self.send_expect(batch, ToWrite::PublishDefault, FromWrite::Published).await
    }

    pub async fn unpublish<I: IntoIterator<Item = Path>>(&self, batch: I) -> Result<()> {
        self.send_expect(batch, ToWrite::Unpublish, FromWrite::Unpublished).await
    }

    pub async fn clear(&self) -> Result<()> {
        let mut batch = RAWTOWRITEPOOL.take();
        batch.push(ToWrite::Clear);
        let r = self.0.send(&batch).await?;
        if r.len() != 1 {
            bail!("unexpected response to clear command {:?}", r)
        } else {
            match &r[0] {
                FromWrite::Unpublished => Ok(()),
                m => bail!("unexpected response to clear command {:?}", m),
            }
        }
    }

    pub(crate) fn secrets(
        &self,
    ) -> Arc<RwLock<HashMap<SocketAddr, u128, FxBuildHasher>>> {
        self.0.secrets()
    }
}
