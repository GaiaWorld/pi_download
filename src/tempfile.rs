//! 下载过程中的临时文件
//! 临时文件保存下载数据及分段信息，下载成功后用rename改成file
//! 分段信息文件的内容格式为u64的数组
//! 下载到数据时，会存到临时文件内，并刷新segments


use core::fmt;
use std::ffi::OsString;
use std::mem;
use std::{path::PathBuf, sync::Arc};
use std::io::{ Result as IoResult, ErrorKind, Error};
use pi_share::{ShareMutex};
use pi_async_file::file::{WriteOptions, AsyncFileOptions};
use pi_rt_file::{SafeFile, remove_file, rename};

use crate::utils;

/// 下载临时文件的后缀名
pub const TMP_SUFFIX_FILE_NAME: &'static str = ".tmp.dl";
/// 分段信息文件的后缀名
pub const SEG_SUFFIX_FILE_NAME: &'static str = ".seg.dl";
/// 分段信息文件的后缀名
pub const DATA_LIMIT: u64 = 256*1024*1024;

/// 下载文件
#[derive(Debug)]
pub struct FileInfo {
    // 文件的目录
    pub dir: PathBuf,
    // 文件名
    pub file: PathBuf,
    // 临时目录, 如果没有临时目录，则直接使用file所在目录。
    pub temp_dir: PathBuf,
    // 内存中保留的下载数据最大大小， 单位为兆， 默认256
    pub data_limit: usize,
}


/// 下载过程中使用的临时文件， 临时文件保存下载，然后用rename改成file
pub struct TempFile {
    // 文件信息
    pub info: FileInfo,
    /// 下载临时文件
    pub temp_file: PathBuf,
    /// 分段信息文件
    pub seg_file: PathBuf,
    /// 下载临时文件的句柄
    temp_file_handle: SafeFile,
    /// 分段信息文件的句柄
    seg_file_handle: SafeFile,
    /// 内存中保留的全部或部分下载数据，(文件中的位置, 数据)，减少再次读取的IO TODO 改成Bytes
    pub data: ShareMutex<(u64, Vec<u8>)>,
    // pub data: ShareMutex<(u64, Vec<(Box<[u8]>, u64)>)>, TODO
}
impl fmt::Debug for TempFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let pos_len = {
            let data = self.data.lock();
            (data.0, data.1.len())
        };
        f.debug_struct("TempFile").field("info", &self.info).field("data", &pos_len).finish()
    }
}
impl TempFile {
    /// 打开临时文件， 返回分段信息
    pub async fn open(info: FileInfo) -> IoResult<(Self, Vec<u8>)> {
        // 获得所在目录
        let dir = if info.temp_dir.as_path().as_os_str().is_empty() {
            info.dir.as_path()
        }else{
            info.temp_dir.as_path()
        };
        // 获得文件名
        let mut temp_file = dir.join(&info.file);
        let mut seg_file = temp_file.clone();
        if let Some(ext) = temp_file.extension() {
            let mut s = OsString::from(ext);
            let mut s1 = s.clone();
            s.push(TMP_SUFFIX_FILE_NAME);
            temp_file.set_extension(s);
            s1.push(SEG_SUFFIX_FILE_NAME);
            seg_file.set_extension(s1);
        }else{
            temp_file.set_extension(TMP_SUFFIX_FILE_NAME);
            seg_file.set_extension(SEG_SUFFIX_FILE_NAME);
        }
        let b = seg_file.exists();
        
        // 创建句柄
        let temp_file_handle = SafeFile::open(temp_file.clone(), AsyncFileOptions::OnlyWrite).await?;
        // 创建句柄
        let seg_file_handle = SafeFile::open(seg_file.clone(), AsyncFileOptions::ReadWrite).await?;
        let file = TempFile {
            info,
            temp_file,
            seg_file,
            temp_file_handle,
            seg_file_handle,
            data: ShareMutex::new((0, vec![])),
        };
        // 读取分段信息
        let v = if b {
            // 读取文件
            let meta = file.seg_file.metadata()?;
            file.seg_file_handle.read(0, meta.len() as usize).await?
        }else{vec![]};
        Ok((file, v))
        
    }
    /// 写入分段信息
    pub async fn write_segments(&self, segments: Vec<u8>) -> IoResult<()> {
        let _ = self.seg_file_handle.write(0, Arc::from(segments.as_slice()),  WriteOptions::None).await?;
        Ok(())
    }
    /// 写入数据
    pub async fn write_data(&self, pos: u64, buf: Box<[u8]>, len: usize) -> IoResult<()> {
        {
            // 内存缓存
            let mut data = self.data.lock();
            if data.1.len() == 0 {
                // 第一次写入
                // 记录起始位置
                data.0 = pos;
                utils::write(&mut data.1, 0, &buf[..len], 0);
            }else if pos >= data.0 { //  && (pos + len as u64) < self.data_limit
                // 追加写入  && pos == data.0 + data.1.len() as u64
                let pos = (pos - data.0) as usize;
                utils::write(&mut data.1, pos, &buf[..len], 0);
            }
        }
        // TODO buf[..len]
        let buf = Arc::from(&buf[..len]);
        let _ = self.temp_file_handle.write(pos, buf, WriteOptions::None).await?;
        Ok(())
    }
    /// 正常结束，删除cfg，将临时文件重命名成下载文件 
    pub async fn finished(&self) -> IoResult<()> {
        // 删除cfg
        remove_file(self.seg_file.clone()).await?;
        // 移动到目标文件
        let dir = self.info.dir.as_path();
        let file = dir.join(&self.info.file);
        rename(self.temp_file.clone(), file).await
    }
    /// 获取文件的二进制数据
    pub async fn take(&self) -> IoResult<Vec<u8>> {
        let dir = self.info.dir.as_path();
        let file = dir.join(&self.info.file);
        if !file.exists() {
            return Err(Error::new(
                ErrorKind::NotFound,
                format!("take fail, file: {:?}", file),
            ))
        }
        let meta = file.metadata()?;
        let mut data = self.data.lock();
        if data.0 == 0 && data.1.len() as u64 == meta.len() {
            return Ok(mem::replace(&mut data.1, vec![]))
        }
        todo!()
    }
}