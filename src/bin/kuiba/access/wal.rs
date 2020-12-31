// Copyright 2020 <盏一 w@hidva.com>
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
// http://www.apache.org/licenses/LICENSE-2.0
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
use crate::guc::{self, GucState};
use kuiba;
use log;
use nix::libc::off_t;
use nix::sys::uio::IoVec;
use nix::unistd::SysconfVar::IOV_MAX;
use std::cmp::min;
use std::fs::{File, OpenOptions};
use std::num::{NonZeroU32, NonZeroU64};
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::thread::panicking;

#[cfg(target_os = "linux")]
fn pwritev(fd: RawFd, iov: &[IoVec<&[u8]>], offset: off_t) -> nix::Result<usize> {
    use nix::sys::uio::pwritev as _pwritev;
    _pwritev(fd, iov, offset)
}

#[cfg(target_os = "macos")]
fn pwritev(fd: RawFd, iov: &[IoVec<&[u8]>], offset: off_t) -> nix::Result<usize> {
    use nix::sys::uio::pwrite;
    let mut buff = Vec::<u8>::new();
    for iv in iov {
        buff.extend_from_slice(iv.as_slice());
    }
    pwrite(fd, buff.as_slice(), offset)
}

fn pwritevn<'a>(
    fd: RawFd,
    iov: &'a mut [IoVec<&'a [u8]>],
    mut offset: off_t,
) -> nix::Result<usize> {
    let orig_offset = offset;
    let iovmax = IOV_MAX as usize;
    let iovlen = iov.len();
    let mut sidx: usize = 0;
    while sidx < iovlen {
        let eidx = min(iovlen, sidx + iovmax);
        let wplan = &mut iov[sidx..eidx];
        let mut part = pwritev(fd, wplan, offset)?;
        offset += part as off_t;
        for wiov in wplan {
            let wslice = wiov.as_slice();
            let wiovlen = wslice.len();
            if wiovlen > part {
                let wpartslice = unsafe {
                    std::slice::from_raw_parts(wslice.as_ptr().add(part), wiovlen - part)
                };
                *wiov = IoVec::from_slice(wpartslice);
                break;
            }
            sidx += 1;
            part -= wiovlen;
            if part <= 0 {
                break;
            }
        }
    }
    Ok((offset - orig_offset) as usize)
}

struct Progress {
    pt: Mutex<kuiba::ProgressTracker>,
    p: kuiba::Progress,
}

impl Progress {
    fn new(d: u64) -> Progress {
        Progress {
            pt: Mutex::new(kuiba::ProgressTracker::new(d)),
            p: kuiba::Progress::new(d),
        }
    }

    fn wait(&self, p: u64) {
        self.p.wait(p)
    }

    fn done(&self, start: u64, end: u64) {
        let np = {
            let mut pt = self.pt.lock().unwrap();
            pt.done(start, end)
        };
        if let Some(np) = np {
            self.p.set(np)
        }
    }

    fn get(&self) -> u64 {
        self.p.get()
    }
}

struct AbortWhenPanic;

impl Drop for AbortWhenPanic {
    fn drop(&mut self) {
        if panicking() {
            std::process::abort();
        }
    }
}

pub type Lsn = NonZeroU64;
pub type TimeLineID = NonZeroU32;

struct WritingWalFile {
    fd: File,
    start_lsn: Lsn,
    write: &'static Progress,
    flush: &'static Progress,
}

fn wal_filepath(tli: TimeLineID, lsn: Lsn) -> String {
    format!("kb_wal/{}/{}", tli, lsn)
}

impl WritingWalFile {
    fn new(
        tli: TimeLineID,
        lsn: Lsn,
        write: &'static Progress,
        flush: &'static Progress,
    ) -> std::io::Result<WritingWalFile> {
        Ok(WritingWalFile {
            fd: WritingWalFile::open_file(tli, lsn)?,
            start_lsn: lsn,
            write,
            flush,
        })
    }

    fn open_file(tli: TimeLineID, lsn: Lsn) -> std::io::Result<File> {
        OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(wal_filepath(tli, lsn))
    }

    fn fsync(&self, end_lsn: u64) -> std::io::Result<()> {
        self.fd.sync_data()?;
        let start_lsn = self.start_lsn.get();
        self.flush.done(start_lsn, end_lsn);
        Ok(())
    }
}

impl Drop for WritingWalFile {
    fn drop(&mut self) {
        if panicking() {
            return;
        }
        let filesize = match self.fd.metadata() {
            Ok(md) => md.len(),
            Err(e) => {
                let errmsg = format!(
                    "WritingWalFile::drop get metadata failed. lsn={} err={}",
                    self.start_lsn, e
                );
                log::error!("{}", errmsg);
                panic!("{}", errmsg);
            }
        };
        let end_lsn = self.start_lsn.get() + filesize;
        if end_lsn > self.write.get() {
            let errmsg = format!(
                "WritingWalFile::drop some writes failed. lsn={}",
                self.start_lsn
            );
            log::error!("{}", errmsg);
            panic!("{}", errmsg);
        }
        if let Err(e) = self.fsync(end_lsn) {
            let errmsg = format!(
                "WritingWalFile::drop sync_data failed. lsn={} err={}",
                self.start_lsn, e
            );
            log::error!("{}", errmsg);
            panic!("{}", errmsg);
        }
    }
}

type RecordBuff = Vec<u8>;

struct InsertWriteReq {
    buf: Vec<RecordBuff>,
    record: Option<RecordBuff>,
    buflsn: Lsn,
    file: Arc<WritingWalFile>,
}

impl InsertWriteReq {
    fn write(self) -> nix::Result<usize> {
        let mut iovec = Vec::with_capacity(self.buf.len() + 1);
        for ref onebuf in &self.buf {
            iovec.push(IoVec::from_slice(onebuf.as_slice()));
        }
        if let Some(ref record) = self.record {
            iovec.push(IoVec::from_slice(record.as_slice()));
        }
        let fd = self.file.fd.as_raw_fd();
        let buflsn = self.buflsn.get();
        let iovec = iovec.as_mut_slice();
        let off = (buflsn - self.file.start_lsn.get()) as off_t;
        let writen = pwritevn(fd, iovec, off)?;
        self.file.write.done(buflsn, buflsn + writen as u64);
        Ok(writen)
    }
}

struct InsertState {
    curtimeline: TimeLineID,
    wal_buff_max_size: usize,
    wal_file_max_size: u64,
    redo: Lsn,
    buf: Vec<RecordBuff>,
    buflsn: Lsn,
    bufsize: usize,
    forcesync: bool,
    // if file is None, it means that file_start_lsn = buflsn.
    file: Option<Arc<WritingWalFile>>,
}

enum InsertRet {
    WriteAndCreate {
        tli: TimeLineID,
        retlsn: Lsn,
        wreq: InsertWriteReq,
    },
    Write(Lsn, InsertWriteReq),
    NoAction(Lsn),
}

impl InsertState {
    fn swap_buff(
        &mut self,
        file: Arc<WritingWalFile>,
        record: Option<RecordBuff>,
        newbuflsn: Lsn,
    ) -> InsertWriteReq {
        let writereq = InsertWriteReq {
            buf: std::mem::replace(&mut self.buf, Vec::new()),
            record,
            buflsn: self.buflsn,
            file,
        };
        self.buflsn = newbuflsn;
        self.bufsize = 0;
        writereq
    }

    // Remeber we are locking, so be quick.
    fn insert(&mut self, record: RecordBuff) -> InsertRet {
        let newbufsize = self.bufsize + record.len();
        let retlsnval = self.buflsn.get() + newbufsize as u64;
        let retlsn = Lsn::new(retlsnval).unwrap();
        if let Some(ref file) = self.file {
            let newfilesize = retlsnval - file.start_lsn.get();
            if newfilesize >= self.wal_file_max_size {
                let file = std::mem::replace(&mut self.file, None).unwrap();
                let wreq = self.swap_buff(file, Some(record), retlsn);
                let ret = InsertRet::WriteAndCreate {
                    tli: self.curtimeline,
                    retlsn,
                    wreq,
                };
                return ret;
            }
            if newbufsize >= self.wal_buff_max_size {
                let file = Arc::clone(file);
                let writereq = self.swap_buff(file, Some(record), retlsn);
                return InsertRet::Write(retlsn, writereq);
            }
        }
        self.bufsize = newbufsize;
        self.buf.push(record);
        return InsertRet::NoAction(retlsn);
    }

    fn nextlsn(&self) -> Lsn {
        Lsn::new(self.buflsn.get() + self.bufsize as u64).unwrap()
    }
}

// Since flush will be referenced by insert.file, for convenience, we make it as a static variable,
// otherwise, facilities like Pin + unsafe will be used.
pub struct GlobalStateExt {
    // redo is the value of insert.redo at a past time.
    redo: AtomicU64,
    insert: Mutex<InsertState>,
    write: &'static Progress,
    flush: &'static Progress,
}

enum FlushAction {
    Noop,
    Wait,
    Flush(Weak<WritingWalFile>),
    Write(InsertWriteReq),
}

impl GlobalStateExt {
    // We make the type of return value as a static ref to tell the caller that
    // you should call this method only once.
    fn new(
        tli: TimeLineID,
        lsn: Lsn,
        redo: Lsn,
        wal_buff_max_size: usize,
        wal_file_max_size: u64,
    ) -> std::io::Result<&'static GlobalStateExt> {
        let flush: &'static Progress = Box::leak(Box::new(Progress::new(lsn.get())));
        let write: &'static Progress = Box::leak(Box::new(Progress::new(lsn.get())));
        Ok(Box::leak(Box::new(GlobalStateExt {
            redo: AtomicU64::new(redo.get()),
            write,
            flush,
            insert: Mutex::new(InsertState {
                wal_buff_max_size,
                wal_file_max_size,
                redo,
                curtimeline: tli,
                buf: Vec::new(),
                buflsn: lsn,
                bufsize: 0,
                forcesync: false,
                file: Some(Arc::new(WritingWalFile::new(tli, lsn, write, flush)?)),
            }),
        })))
    }

    fn get_insert_state(&self) -> MutexGuard<InsertState> {
        let insert = self.insert.lock().unwrap();
        self.redo.store(insert.redo.get(), Ordering::Relaxed);
        insert
    }

    fn do_create(&self, tli: TimeLineID, retlsn: Lsn) -> anyhow::Result<()> {
        let file = Arc::new(WritingWalFile::new(tli, retlsn, self.write, self.flush)?);
        let wreq = {
            let mut insert = self.get_insert_state();
            if insert.forcesync {
                let nxtlsn = insert.nextlsn();
                insert.file = Some(file.clone());
                insert.forcesync = false;
                Some(insert.swap_buff(file, None, nxtlsn))
            } else {
                insert.file = Some(file);
                None
            }
        };
        if let Some(wreq) = wreq {
            let weak_file = Arc::downgrade(&wreq.file);
            let filelsn = wreq.buflsn.get();
            let wn = wreq.write()?;
            self.do_fsync(weak_file, filelsn + wn as u64)?;
        }
        Ok(())
    }

    fn handle_insert_ret(&self, ret: InsertRet) -> anyhow::Result<Lsn> {
        match ret {
            InsertRet::NoAction(lsn) => Ok(lsn),
            InsertRet::Write(lsn, wreq) => {
                wreq.write()?;
                Ok(lsn)
            }
            InsertRet::WriteAndCreate { tli, retlsn, wreq } => {
                wreq.write()?;
                self.do_create(tli, retlsn)?;
                Ok(retlsn)
            }
        }
    }

    pub fn insert_record(&self, r: RecordBuff) -> Lsn {
        let _guard = AbortWhenPanic;
        let insert_res = {
            let mut state = self.get_insert_state();
            state.insert(r)
        };
        self.handle_insert_ret(insert_res).unwrap()
    }

    pub fn try_insert_record(&self, r: RecordBuff, page_lsn: Lsn) -> Option<Lsn> {
        let _guard = AbortWhenPanic;
        let insert_res = {
            let mut state = self.get_insert_state();
            if page_lsn <= state.redo {
                return None;
            }
            state.insert(r)
        };
        Some(self.handle_insert_ret(insert_res).unwrap())
    }

    fn flush_action(&self, lsn: Lsn) -> FlushAction {
        let lsnval = lsn.get();
        let mut insert = self.get_insert_state();
        if lsnval <= self.flush.get() {
            return FlushAction::Noop;
        }
        if let Some(ref file) = insert.file {
            if lsn <= file.start_lsn {
                return FlushAction::Wait;
            }
            if lsn <= insert.buflsn {
                return FlushAction::Flush(Arc::downgrade(file));
            }
            let file = file.clone();
            let nxtlsn = insert.nextlsn();
            let wreq = insert.swap_buff(file, None, nxtlsn);
            return FlushAction::Write(wreq);
        }
        if lsn <= insert.buflsn {
            return FlushAction::Wait;
        }
        insert.forcesync = true;
        return FlushAction::Wait;
    }

    fn do_fsync(&self, weak_file: Weak<WritingWalFile>, lsnval: u64) -> std::io::Result<()> {
        let file = weak_file.upgrade();
        if let Some(file) = file {
            self.write.wait(lsnval);
            file.fsync(lsnval)?;
        }
        Ok(self.flush.wait(lsnval))
    }

    pub fn fsync(&self, lsn: Lsn) -> std::io::Result<()> {
        let _guard = AbortWhenPanic;
        let lsnval = lsn.get();
        if lsnval <= self.flush.get() {
            return Ok(());
        }
        let action = self.flush_action(lsn);
        match action {
            FlushAction::Noop => Ok(()),
            FlushAction::Wait => Ok(self.flush.wait(lsnval)),
            FlushAction::Flush(weak_file) => self.do_fsync(weak_file, lsnval),
            FlushAction::Write(wreq) => {
                let weak_file = Arc::downgrade(&wreq.file);
                wreq.write().unwrap();
                self.do_fsync(weak_file, lsnval)
            }
        }
    }
}

pub fn init(
    tli: TimeLineID,
    lsn: Lsn,
    redo: Lsn,
    gucstate: &GucState,
) -> std::io::Result<&'static GlobalStateExt> {
    let wal_buff_max_size = guc::get_int(gucstate, guc::WalBuffMaxSize) as usize;
    let wal_file_max_size = guc::get_int(gucstate, guc::WalFileMaxSize) as u64;
    GlobalStateExt::new(tli, lsn, redo, wal_buff_max_size, wal_file_max_size)
}
