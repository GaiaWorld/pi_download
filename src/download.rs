//! TODO 用基于 blake3 的 merkle tree来记录文件hash，https://rmw.link/zh/log/2022-06-02-blake3_merkle.html，这样在分段下载和断点续连中，可尽量减少文件读取IO和内存占用
//! TODO 用AsyncBytes来作为文件下载后的结果，这样可以尽量减少读取

use async_httpc::{AsyncHttpRequestMethod, AsyncHttpResponse, AsyncHttpc};
use bytes::Buf;
use flume::{bounded, Sender};
use futures::future::BoxFuture;
use futures::FutureExt;
use pi_async::rt::{local_async_runtime, spawn_local, AsyncTaskPool, AsyncTaskPoolExt};
use pi_enum_default_macro::EnumDefault;
use pi_share::{Share, ShareMutex, ShareUsize};
use pi_time::now_millisecond;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::VecDeque;
use std::io::{self, Error, ErrorKind, Result as IoResult};
use std::ops::Range;
use std::sync::atomic::Ordering;
use std::{fmt, mem};

use crate::tempfile::TempFile;
use crate::utils;

pub const PARALLEL_SIZE: usize = 3 * 1024 * 1024;
pub const RE_CONNECT_SIZE: usize = 1 * 1024 * 1024;
pub const TIMEOUT: usize = 16000;
pub const RETRY: usize = 3;
const SPEED_SAMPLING: usize = 5;
const PARALLEL_CHECK_INTERVAL: u64 = 3000;
const PARALLEL_CHECK_SPEED_INCR: f64 = 1.2;

pub trait Notify: Clone + Send + Sync + 'static {
    fn notify(&self, download: &Share<Download>, result: &IoResult<()>);
}
/// 空下载通知器
#[derive(Clone)]
pub struct EmptyNotify();
impl Notify for EmptyNotify {
    fn notify(&self, _download: &Share<Download>, _result: &IoResult<()>) {}
}

#[derive(Clone, EnumDefault, PartialEq, Debug)]
pub enum SignVerify {
    None,
    Adler32([u8; 4]),
    Crc32([u8; 4]),
    Crc64([u8; 8]),
    Md5([u8; 16]),
    Sha1([u8; 20]),
}
/// 线程安全的下载器
#[derive(Debug)]
pub struct Download {
    /// 下载的信息
    pub info: Info,
    /// 下载的状态
    state: ShareMutex<State>,
    /// 下载的存储
    storage: Storage,
    /// 关联信息
    pub link: Value,
    /// 本次下载启动的毫秒时间
    pub start_time_ms: u64,
    /// 已经尝试的次数
    retry_count: ShareUsize,
}
impl Download {
    fn new(info: Info, storage: Storage, segments: Vec<u8>, link: Value) -> Share<Self> {
        let now = now_millisecond();
        let mut seg = vec![];
        let mut buf = segments.as_slice();
        while buf.len() >= 2 * (u64::BITS / u8::BITS) as usize {
            let start = buf.get_u64_le();
            let end = buf.get_u64_le();
            seg.push((Range { start, end }, false));
        }
        Share::new(Self {
            info,
            state: ShareMutex::new(State::new(seg, now)),
            storage,
            link,
            start_time_ms: now,
            retry_count: ShareUsize::new(0),
        })
    }
    pub fn down_file(info: Info, file: TempFile, segments: Vec<u8>, link: Value) -> Share<Self> {
        Download::new(info, Storage::File(file), segments, link)
    }
    pub fn down_data(info: Info, segments: Vec<u8>, link: Value) -> Share<Self> {
        Download::new(info, Storage::Mem(Default::default()), segments, link)
    }
    pub fn get_state(&self) -> State {
        self.state.lock().clone()
    }
    pub fn get_retry_count(&self) -> usize {
        self.retry_count.load(Ordering::Relaxed)
    }
    pub async fn take_data(&self) -> IoResult<Vec<u8>> {
        self.storage.take().await
    }
    pub async fn stop(&self, e: Error) -> IoResult<()> {
        let s = {
            let mut s = self.state.lock();
            s.sender.take()
        };
        over(s, Err(e)).await
    }
    pub async fn start<P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>, T: Notify>(
        d: &Share<Self>,
        httpc: AsyncHttpc,
        notify: T,
    ) -> IoResult<()> {
        let receiver = {
            let mut s = d.state.lock();
            if s.sender.is_some() {
                return Err(io::Error::new(io::ErrorKind::Other, "invalid status"));
            }
            let (sender, receiver) = bounded(1);
            s.sender = Some(sender);
            receiver
        };
        d.retry_count.store(0, Ordering::SeqCst);
        let self1 = d.clone();
        let notify1 = notify.clone();
        spawn_local::<(), _>(async move {
            notify.notify(&self1, &Ok(()));
            let _ = Download::request::<P, _>(self1, httpc, notify).await;
        })?;
        match receiver.recv_async().await {
            Ok(r) => {
                if r.is_ok() {
                    d.storage.finished().await?;
                }
                notify1.notify(&d, &r);
                r
            }
            Err(e) => {
                //接收错误，则立即返回
                Err(Error::new(
                    ErrorKind::Other,
                    format!("request fail, reason: {:?}", e),
                ))
            }
        }
    }
    /// 下载请求
    async fn request<P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>, T: Notify>(
        d: Share<Self>,
        httpc: AsyncHttpc,
        notify: T,
    ) -> IoResult<()> {
        loop {
            // 获取指定的区间
            let range = d.get_range();
            if range.is_empty() {
                // 如果没有找到可以下载的区间，则返回
                return Ok(());
            }
            let req = if range.end == u64::MAX {
                // 首次下载
                httpc.build_request(d.info.url.as_str(), AsyncHttpRequestMethod::Get)
            } else {
                // 分段下载
                let req = httpc.build_request(d.info.url.as_str(), AsyncHttpRequestMethod::Get);
                let s = format!("bytes={}-{}", range.start, range.end - 1);
                // println!("resp_range: Range:{:?}", s);
                req.add_header("Range", &s)
            };
            match req.set_timeout(d.info.timeout as u64).send().await {
                Ok(resp) => {
                    // println!("resp!==========code:{:?} len:{:?}", resp.get_status(), resp.get_body_len());
                    // TODO If-Range 和 Etag的匹配， 处理301和302
                    let status = resp.get_status();
                    if status == 200 {
                        // TODO 如果不支持206，则顺序下载
                    } else if status == 206 {
                    } else {
                        if let Some(r) = Download::error(
                            &d,
                            &range,
                            Error::new(
                                ErrorKind::Other,
                                format!("request fail, status: {:?}", status),
                            ),
                            &notify,
                        )
                        .await
                        {
                            return r;
                        }
                    }
                    if range.end == u64::MAX {
                        // 如果是初次下载，则进行长度检查
                        if let Some(body_len) = resp.get_body_len() {
                            if let Some(size) = d.info.size {
                                if body_len != size {
                                    return Err(Error::new(
                                        ErrorKind::InvalidInput,
                                        "mismatched file size",
                                    ));
                                }
                            }
                            // 修正范围大小
                            d.set_range(body_len)
                        }
                    }
                    // let (range, result) = self.body::<P, _>(resp, range, &httpc, &notify).await;
                    // 由于编译器的bug ，没法直接调用body方法，所以用同步函数包一下来调用
                    let f = Download::body1::<P, T>(
                        d.clone(),
                        resp,
                        range,
                        httpc.clone(),
                        notify.clone(),
                    );
                    let (range, result) = f.await;
                    match result {
                        Err(e) => {
                            if let Some(r) = Download::error(&d, &range, e, &notify).await {
                                return r;
                            }
                        }
                        Ok(_) => continue,
                    }
                }
                Err(e) => {
                    if let Some(r) = Download::error(&d, &range, e, &notify).await {
                        return r;
                    }
                }
            }
        }
    }
    /// 由于编译器的bug ，没法直接调用body方法，所以用同步函数包一下来调用
    fn body1<P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>, T: Notify>(
        d: Share<Self>,
        resp: AsyncHttpResponse,
        range: Range<u64>,
        httpc: AsyncHttpc,
        notify: T,
    ) -> BoxFuture<'static, (Range<u64>, IoResult<()>)> {
        (async move { Download::body::<P, T>(d, resp, range, &httpc, &notify).await }).boxed()
    }
    async fn body<P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>, T: Notify>(
        d: Share<Self>,
        mut resp: AsyncHttpResponse,
        mut range: Range<u64>,
        httpc: &AsyncHttpc,
        notify: &T,
    ) -> (Range<u64>, IoResult<()>) {
        loop {
            // 判断是否增加新的下载
            if d.can_parallel() {
                let rt = local_async_runtime::<()>().unwrap();
                let httpc = httpc.clone();
                let notify1 = notify.clone();
                let self1 = d.clone();
                // let id = rt.alloc();
                match rt.spawn(async move {
                    let _ = Download::request::<P, T>(self1, httpc, notify1).await;
                }) {
                    Err(e) => return (range, Err(e)),
                    _ => (),
                }
            }
            match resp.get_body().await {
                Err(e) => return (range, Err(e)),
                Ok(Some(data)) => {
                    let data_len = data.len();
                    let over_len = d.down_ok(&range, data_len);
                    // notify.notify(&d, &Ok(()));
                    if over_len == usize::MAX {
                        return (range, Ok(()));
                    }
                    if d.storage.is_file() {
                        // 写入存储, spawn写 TODO 还是改回不开future
                        let rt = local_async_runtime::<()>().unwrap();
                        let notify1 = notify.clone();
                        let pos = range.start;
                        let self1 = d.clone();
                        // let id = rt.alloc();
                        match rt.spawn(async move {
                            let _ = Download::save(&self1, pos, data, over_len, &notify1).await;
                        }) {
                            Err(e) => return (range, Err(e)),
                            _ => (),
                        }
                    } else {
                        match Download::save(&d, range.start, data, over_len, notify).await {
                            Err(e) => return (range, Err(e)),
                            _ => (),
                        }
                    }
                    if over_len > 0 {
                        return (range, Ok(()));
                    }
                    // 继续下载
                    range.start += data_len as u64;
                }
                Ok(None) => return (range, Ok(())),
            }
        }
    }
    async fn error<T: Notify>(
        d: &Share<Self>,
        range: &Range<u64>,
        e: Error,
        notify: &T,
    ) -> Option<IoResult<()>> {
        let (stop, sender) = d.down_err(&range);
        if stop.is_none() {
            // 下载已经被别的future结束
            return Some(Ok(()));
        }
        let e = Err(e);
        if sender.is_none() {
            // 通知下载异常
            notify.notify(d, &e);
        }
        if stop.unwrap() {
            // 尝试次数已达到最大上限，返回错误
            return Some(over(sender, e).await);
        }
        None
    }
    async fn save<T: Notify>(
        d: &Share<Self>,
        pos: u64,
        data: Box<[u8]>,
        over_len: usize,
        notify: &T,
    ) -> IoResult<()> {
        let len = if over_len > 0 { over_len } else { data.len() };
        // 处理本地磁盘写入失败
        match d.storage.save(d, pos, data, len).await {
            Ok(_) => {
                // 通知下载进度
                notify.notify(d, &Ok(()));
                if over_len > 0 {
                    // 分段下载结束
                    let sender = d.over_sender();
                    // if sender.is_none() {
                    //     // 通知下载进度
                    //     notify.notify(d, &Ok(()));
                    // }
                    return over(sender, Ok(())).await;
                }
                Ok(())
            }
            Err(e) => d.stop(e).await,
        }
    }
    /// 初次下载，设置文件范围大小
    fn set_range(&self, size: u64) {
        let mut state = self.state.lock();
        state.segments[0] = (0..size, true);
    }
    /// 获得文件大小
    pub fn file_size(&self) -> Option<u64>{
        let state = self.state.lock();
        state.file_size()
    }
    /// 获得查询所对应的当前的范围
    fn get_range(&self) -> Range<u64> {
        let mut state = self.state.lock();
        state.get_range(self.info.parallel_size)
    }
    // 下载失败， 取消下载标识，返回是否停止请求
    fn down_err(&self, r: &Range<u64>) -> (Option<bool>, Option<Sender<IoResult<()>>>) {
        let mut state = self.state.lock();
        if state.sender.is_none() {
            // 下载已经结束
            return (None, None);
        }
        let parallel = state.down_err(r);
        // 重试次数为： retry*分段下载数parallel+retry
        let retry_count = self.retry_count.fetch_add(1, Ordering::SeqCst);
        let b = retry_count >= self.info.retry * (parallel as usize + 1);
        // 如果已经无法尝试，并且所有下载都结束，则返回sender
        (
            Some(b),
            if b && parallel == 0 {
                state.sender.take()
            } else {
                None
            },
        )
    }
    // 下载成功，返回usize::MAX表示已经结束， 返回0表示继续， 返回非0表示数据需要被截断并停止
    fn down_ok(&self, r: &Range<u64>, data_len: usize) -> usize {
        let mut state = self.state.lock();
        if state.sender.is_none() {
            return usize::MAX;
        }
        state.down_ok(r, data_len, now_millisecond() - self.start_time_ms)
    }
    // 判断是否结束， 如果结束，则尝试获取Sender
    fn over_sender(&self) -> Option<Sender<IoResult<()>>> {
        let mut state = self.state.lock();
        if state.parallel() == 0 {
            state.sender.take()
        } else {
            None
        }
    }
    // 判断是否增加新的下载
    fn can_parallel(&self) -> bool {
        let now = now_millisecond();
        let mut state = self.state.lock();
        if state.segments.len() == 1 {
            state.last_check_time = now + PARALLEL_CHECK_INTERVAL;
            // 如果只有一个下载， 并且有body_len，则新增下载
            return state.segments[0].0.end != u64::MAX;
        }
        if state.last_check_time > now {
            return false;
        }
        state.last_check_time = now + PARALLEL_CHECK_INTERVAL;
        let s = state.speed();
        // 如果当前速度大于上次新增下载前的速度，则继续新增下载
        if s <= state.last_check_speed as usize {
            return false;
        }
        // 记录最新的速度*1.2，然后新增下载
        state.last_check_speed = (s as f64) * PARALLEL_CHECK_SPEED_INCR;
        true
    }
    // 获得分段信息
    fn get_segments(&self) -> Vec<u8> {
        let state = self.state.lock();
        state
            .segments
            .iter()
            .flat_map(|s| {
                s.0.start
                    .to_le_bytes()
                    .into_iter()
                    .chain(s.0.end.to_le_bytes().into_iter())
            })
            .collect()
    }
}

/// 2种下载存储类型 TODO 改成Bytes，可以先用Vec<(pos, Box<[u8]>)>来存放，下载完毕后排序变成Bytes
pub enum Storage {
    File(TempFile),
    Mem(ShareMutex<Vec<u8>>),
}
impl fmt::Debug for Storage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Storage::File(file) => write!(f, "Storage::File({:?})", file),
            Storage::Mem(v) => {
                let len = { v.lock().len() };
                write!(f, "Storage::Mem(Vec({}))", len)
            }
        }
    }
}
impl Storage {
    pub fn is_file(&self) -> bool {
        match &self {
            Storage::File(_) => true,
            _ => false,
        }
    }
    pub async fn take(&self) -> IoResult<Vec<u8>> {
        match &self {
            Storage::File(file) => file.take().await,
            Storage::Mem(mutex) => {
                let mut vec = mutex.lock();
                Ok(mem::replace(&mut vec, vec![]))
            }
        }
    }
    pub async fn save(
        &self,
        d: &Share<Download>,
        pos: u64,
        buf: Box<[u8]>,
        buf_len: usize,
    ) -> IoResult<()> {
        match &self {
            Storage::File(file) => {
                file.write_data(pos, buf, buf_len).await?;
                let s = d.get_segments();
                if s.len() > 0 {
                    file.write_segments(s).await
                } else {
                    Ok(())
                }
            }
            Storage::Mem(mutex) => {
                let mut vec = mutex.lock();
                utils::write(&mut vec, pos as usize, &buf[..buf_len], 0);
                Ok(())
            }
        }
    }
    pub async fn finished(&self) -> IoResult<()> {
        match &self {
            Storage::File(file) => file.finished().await,
            Storage::Mem(_) => Ok(()),
        }
    }
}
/// TODO ETag签名，
#[derive(Debug)]
pub struct Info {
    /// url地址
    pub url: String,
    /// 数据签名， 如果没有，则下载后不验证
    pub sign: SignVerify,
    /// 数据大小， 如果没有， 则尽量通过头信息来获取， 如果无法获取， 则无法断点续连或并发下载
    pub size: Option<u64>,
    /// 启用并行下载的数据大小， 单位为兆， 默认3
    pub parallel_size: usize,
    /// 启用断线续连的数据大小， 单位为兆， 默认1
    pub re_connect_size: usize,
    /// 下载过程的超时， 超时则重新建立连接进行尝试
    pub timeout: usize,
    /// 最大尝试下载的次数
    pub retry: usize,
}
impl Info {
    pub fn new(url: String) -> Self {
        Self {
            url,
            sign: Default::default(),
            size: None,
            parallel_size: PARALLEL_SIZE,
            re_connect_size: RE_CONNECT_SIZE,
            timeout: TIMEOUT,
            retry: RETRY,
        }
    }
    pub fn with_config(url: String, size: Option<u64>, timeout: usize, retry: usize) -> Self {
        Self {
            url,
            sign: Default::default(),
            size,
            parallel_size: PARALLEL_SIZE,
            re_connect_size: RE_CONNECT_SIZE,
            timeout,
            retry,
        }
    }
}
#[derive(Clone, Default, Debug)]
pub struct State {
    /// 并发下载时，等待下载的分段信息，(Range(空闲块的开始位置, 空闲块的结束位置), 当前是否正在被下载)
    pub segments: Vec<(Range<u64>, bool)>,
    /// 本次下载的总大小
    pub down_size: u64,
    /// 已下载的总大小
    pub load_size: u64,
    /// 通知主future的发送器
    sender: Option<Sender<IoResult<()>>>,
    /// 最近N秒的下载速度, (和开始时间的毫秒时间差，下载的数据量)
    pub speeds: VecDeque<(u64, usize)>,
    /// 最近N秒的下载的大小
    pub speeds_size: usize,
    /// 最近一次检查是否分区的时间， 和开始时间的毫秒时间差
    pub last_check_time: u64,
    /// 最近一次检查时的速度
    pub last_check_speed: f64,
}
impl State {
    fn new(segments: Vec<(Range<u64>, bool)>, now: u64) -> Self {
        // 获取已下载的大小
        let mut n = 0;
        let mut last = 0;
        for r in segments.iter() {
            n += r.0.start - last;
            last = r.0.end;
        }
        Self {
            segments,
            down_size: 0,
            load_size: n,
            sender: None,
            speeds: Default::default(),
            speeds_size: 0,
            last_check_time: now + PARALLEL_CHECK_INTERVAL,
            last_check_speed: 0.0,
        }
    }
    pub fn is_running(&self) -> bool {
        self.sender.is_some()
    }
    /// 获得每秒速度
    pub fn speed(&self) -> usize {
        let mut time = match self.speeds.front() {
            Some(s) => s.0,
            _ => return 0,
        };
        let s = self.speeds.back().unwrap();
        time = s.0 - time;
        if time == 0 {
            return 0;
        }
        ((((self.speeds_size - s.1) * 1000) as u64) / time) as usize
    }
    /// 获得并行下载的数量
    pub fn parallel(&self) -> usize {
        let mut n = 0;
        for &(_, b) in self.segments.iter() {
            if b {
                n += 1;
            }
        }
        n
    }
    /// 获得文件大小
    pub fn file_size(&self) -> Option<u64>{
        if self.segments.len() == 0 {
            return None
        }
        Some(self.segments[self.segments.len() - 1].0.end)
    }
    /// 下载失败， 取消下载标识，返回正在下载的分段数
    fn down_err(&mut self, r: &Range<u64>) -> usize {
        let mut n = 0;
        for i in self.segments.iter_mut() {
            if i.1 && i.0.start == r.start {
                i.1 = false;
                continue;
            }
            if i.1 {
                n += 1;
            }
        }
        n
    }
    /// 下载成功，返回0表示继续， 返回非0表示数据需要被截断并停止
    fn down_ok(&mut self, r: &Range<u64>, data_len: usize, duration: u64) -> usize {
        for i in self.segments.iter_mut() {
            if i.0.start != r.start || !i.1 {
                continue;
            }
            let len = (i.0.end - i.0.start) as usize;
            if len > data_len {
                i.0.start += data_len as u64;
                self.set_speed(data_len, duration);
                return 0;
            }
            i.1 = false;
            i.0.start = i.0.end;
            self.set_speed(len, duration);
            return len;
        }
        panic!(
            "down_ok, invalid range:{:?}, data_len:{}, segments:{:?}",
            r, data_len, &self.segments
        )
    }
    /// 设置速度
    fn set_speed(&mut self, data_len: usize, duration: u64) {
        self.down_size += data_len as u64;
        self.load_size += data_len as u64;
        self.speeds_size += data_len;
        if let Some(s) = self.speeds.back_mut() {
            if duration - s.0 < 1000 {
                // 1秒以内，则修改下载量
                s.1 += data_len;
                return;
            }
        }
        // 记录新时间的数据
        self.speeds.push_back((duration, data_len));
        if self.speeds.len() > SPEED_SAMPLING {
            let s = self.speeds.pop_front().unwrap();
            self.speeds_size -= s.1;
        }
    }
    /// 获得查询所对应的当前的范围
    fn get_range(&mut self, parallel_size: usize) -> Range<u64> {
        if self.sender.is_none() {
            // 下载已经结束
            return 0..0;
        }
        if self.segments.len() == 0 {
            self.segments.push((0..u64::MAX, true));
            return 0..u64::MAX;
        }
        // 寻找第一个没有下载的块， 如果没有，则找到最大空闲块，判断是否需要二分该块
        // 标记块为下载状态
        let mut index = 0;
        let mut max = 0;
        for i in 0..self.segments.len() {
            let r = &mut self.segments[i];
            if (!r.1) && !r.0.is_empty() {
                // 找到第一个没有下载的块
                r.1 = true;
                return r.0.clone();
            }
            if r.0.end - r.0.start > max {
                index = i;
                max = r.0.end - r.0.start;
            }
        }
        // 如果最大块不足够大，返回false
        if parallel_size as u64 > max {
            // 同时禁止再尝试分段下载
            self.last_check_time = u64::MAX;
            return 0..0;
        }
        // 如果最大块足够大，可以继续分段下载，则劈分该块
        let mut r = self.segments[index].0.clone();
        let split = (r.start + r.end) / 2;
        self.segments[index].0.end = split;
        r.start = split;
        self.segments.insert(index + 1, (r.clone(), true));
        r
    }
}

#[derive(Serialize, Deserialize, Debug)]
struct Seg {
    x: i32,
    y: i32,
}

/// 尝试给主future发送结束消息
async fn over(s: Option<Sender<IoResult<()>>>, r: IoResult<()>) -> IoResult<()> {
    if let Some(sender) = s {
        return match sender.into_send_async(r).await {
            Ok(_) => Ok(()),
            Err(e) => Err(Error::new(
                ErrorKind::Other,
                format!("into_send_async fail, reason: {:?}", e),
            )),
        };
    }
    r
}
// 使用下载器来获取数据， 主要是可多次重试
pub async fn http_get<P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>>(
    httpc: AsyncHttpc,
    url: String,
    _retry: usize,
) -> IoResult<Vec<u8>> {
    let d = Download::down_data(Info::new(url), vec![], Value::Null);
    match Download::start::<P, _>(&d, httpc, EmptyNotify()).await {
        Ok(_) => d.take_data().await,
        Err(r) => Err(r),
    }
}
///  redundant_count 冗余下载的数量， 多url地址中第一个返回成功为成功。
pub async fn http_gets(_httpc: AsyncHttpc, _urls: Vec<String>) -> IoResult<Vec<u8>> {
    // map_or
    todo!()
}

#[cfg(test)]
mod test_mod {
    use crate::download::*;
    use crate::tempfile::FileInfo;
    use async_httpc::AsyncHttpcBuilder;
    use std::sync::RwLock;
    use pi_async::rt::multi_thread::{
        MultiTaskRuntime, MultiTaskRuntimeBuilder, StealableTaskPool,
    };
    use pi_async::rt::AsyncRuntime;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;
    use std::time::{Duration, Instant};

    use std::time::{SystemTime, UNIX_EPOCH};

    #[derive(Clone)]
    struct G(Arc<RwLock<Instant>>);

    impl Notify for G {
        fn notify(&self, d: &Share<Download>, result: &IoResult<()>) {
            let s = d.get_state();
            if s.is_running() {
                let time = {self.0.read().unwrap().elapsed().as_millis()};

                if time > 1000 {
                    *self.0.write().unwrap() = Instant::now();
                    println!("notify:----------r:{:?} , load:{}, down:{}, file_size:{:?}", result, s.load_size, s.down_size, s.file_size());
                }
                return;
            }
            
        }
    }
    #[test]
    pub fn test() {
        let pool = MultiTaskRuntimeBuilder::default();
        let rt0 = pool.build();
        // let rt1 = rt0.clone();


        let _ = rt0.spawn(rt0.alloc(), async move {
            let cur = std::env::current_dir().unwrap();
            let (file, seg) = TempFile::open(FileInfo { dir: cur, file: PathBuf::from("test.exe"), temp_dir: PathBuf::from(""), data_limit: 10 * 1024 }).await.unwrap();
            println!("reload file:{:?}, seg:{:?}", file, seg);
            let d = Download::down_file(Info::with_config("https://pi-client-cfg.oss-cn-chengdu.aliyuncs.com/exes/test.exe".to_string(), None, 100 * 1000, 3), file, seg, Value::Null);
            let r = match Download::start::<StealableTaskPool<()>, _>(&d, AsyncHttpcBuilder::new().build().unwrap(), G(Arc::new(RwLock::new (Instant::now())))).await {
                Ok(_) => Ok(()),
                Err(r) => Err(r),
            };
            println!("----r: {:?}", r);
        });
        std::thread::sleep(Duration::from_millis(100000000));
    }
}
