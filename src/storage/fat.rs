use std::ffi::{CStr, CString};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::Duration;

use esp_idf_svc::sys::{
    beamer_fat_ro_register, esp, esp_vfs_fat_register, esp_vfs_fat_unregister_path, f_closedir,
    f_mount, f_opendir, f_readdir, f_stat, ff_diskio_get_drive, ff_diskio_register,
    ff_diskio_register_sdmmc, EspError, FATFS, FF_DIR, FILINFO,
};

unsafe fn ff_diskio_release(pdrv: u8) {
    ff_diskio_register(pdrv, core::ptr::null());
}

use super::SdCard;

pub const BASE_PATH: &str = "/sd";
pub const RO_BASE_PATH: &str = "/ro";

const MAX_FILES: usize = 2;

struct Mount {
    pdrv: u8,
    base: CString,
    drive: CString,
    fs: *mut FATFS,
}

impl Mount {
    fn open(
        base_path: &str,
        register: impl FnOnce(u8) -> Result<(), EspError>,
    ) -> Result<Mount, EspError> {
        let mut pdrv: u8 = 0;
        esp!(unsafe { ff_diskio_get_drive(&mut pdrv) })?;

        let drive = CString::new(format!("{pdrv}:")).expect("no interior NUL");
        let base = CString::new(base_path).expect("no interior NUL");

        if let Err(e) = register(pdrv) {
            unsafe { ff_diskio_release(pdrv) };
            return Err(e);
        }

        let mut fs: *mut FATFS = core::ptr::null_mut();
        if let Err(e) =
            esp!(unsafe { esp_vfs_fat_register(base.as_ptr(), drive.as_ptr(), MAX_FILES, &mut fs) })
        {
            unsafe { ff_diskio_release(pdrv) };
            return Err(e);
        }

        let res = unsafe { f_mount(fs, drive.as_ptr(), 1) };
        if res != 0 {
            unsafe {
                esp_vfs_fat_unregister_path(base.as_ptr());
                ff_diskio_release(pdrv);
            }
            log::error!("f_mount({base_path}) failed: FRESULT {res}");
            return Err(EspError::from_infallible::<
                { esp_idf_svc::sys::ESP_ERR_NOT_FOUND },
            >());
        }

        Ok(Mount {
            pdrv,
            base,
            drive,
            fs,
        })
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        unsafe {
            f_mount(core::ptr::null_mut(), self.drive.as_ptr(), 0);
            esp_vfs_fat_unregister_path(self.base.as_ptr());
            ff_diskio_release(self.pdrv);
        }
    }
}

pub struct WriteWindow(Mount);

impl WriteWindow {
    pub fn open(sd: &SdCard) -> Result<WriteWindow, EspError> {
        Mount::open(BASE_PATH, |pdrv| {
            unsafe { ff_diskio_register_sdmmc(pdrv, sd.raw()) };
            Ok(())
        })
        .map(WriteWindow)
    }

    /// `rel` as FatFs itself names it on this window's drive (`0:/SLIPPI`),
    /// for the FatFs calls below.
    pub fn fat_path(&self, rel: &str) -> CString {
        fat_path(&self.0.drive, rel)
    }

    /// The volume's first sector on the card (its boot sector).
    pub fn volume_base(&self) -> u32 {
        // SAFETY: the mount is live as long as the window
        unsafe { (*self.0.fs).volbase }
    }
}

fn fat_path(drive: &CStr, rel: &str) -> CString {
    let drive = drive.to_str().unwrap_or("0:");
    CString::new(format!("{drive}/{rel}")).expect("no interior NUL")
}

/// One directory entry, as FatFs reads it: the FAT date and time with it,
/// which the VFS's `stat` turns into a `time_t`.
pub struct FatEntry<'a> {
    pub name: &'a str,
    pub size: u32,
    pub fdate: u16,
    pub ftime: u16,
    pub dir: bool,
}

const AM_DIR: u8 = 0x10;

/// Walks the directory `path` (a [`WriteWindow::fat_path`] or
/// [`ReadWindow::fat_path`]) with FatFs directly: no `stat` per entry.
/// Errors are FatFs's FRESULT.
pub fn for_each_entry(path: &CStr, mut f: impl FnMut(&FatEntry<'_>)) -> Result<(), u32> {
    // SAFETY: FatFs fills both; all-zero is their documented initial state
    let mut dir: FF_DIR = unsafe { core::mem::zeroed() };
    let mut fno: FILINFO = unsafe { core::mem::zeroed() };
    let res = unsafe { f_opendir(&mut dir, path.as_ptr()) };
    if res != 0 {
        return Err(res as u32);
    }
    let out = loop {
        let res = unsafe { f_readdir(&mut dir, &mut fno) };
        if res != 0 {
            break Err(res as u32);
        }
        if fno.fname[0] == 0 {
            break Ok(());
        }
        // SAFETY: FatFs NUL-terminates fname
        let name = unsafe { CStr::from_ptr(fno.fname.as_ptr()) };
        let Ok(name) = name.to_str() else { continue };
        f(&FatEntry {
            name,
            size: fno.fsize as u32,
            fdate: fno.fdate,
            ftime: fno.ftime,
            dir: fno.fattrib & AM_DIR != 0,
        });
    };
    unsafe { f_closedir(&mut dir) };
    out
}

/// One file's size and FAT modified date and time, or `None` if it is not
/// there.
pub fn stat(path: &CStr) -> Option<(u32, u16, u16)> {
    let mut fno: FILINFO = unsafe { core::mem::zeroed() };
    (unsafe { f_stat(path.as_ptr(), &mut fno) } == 0).then_some((
        fno.fsize as u32,
        fno.fdate,
        fno.ftime,
    ))
}

/// Where the FAT lies on the card, from the mounted volume.
#[derive(Debug, Clone, Copy)]
pub struct FatGeometry {
    /// FS_FAT16 2, FS_FAT32 3
    pub fs_type: u8,
    /// first sector of the first FAT
    pub fat_base: u32,
    /// sectors per FAT
    pub fat_sectors: u32,
    /// clusters + 2
    pub entries: u32,
    /// sectors per cluster
    pub cluster_sectors: u32,
}

struct Gate {
    held: bool,
    scan_waiting: bool,
}

static GATE: Mutex<Gate> = Mutex::new(Gate {
    held: false,
    scan_waiting: false,
});
static FREED: Condvar = Condvar::new();

const SCAN_POLL: Duration = Duration::from_secs(1);

fn gate() -> MutexGuard<'static, Gate> {
    GATE.lock().unwrap_or_else(|e| e.into_inner())
}

struct Held;

impl Held {
    fn claim(mut g: MutexGuard<'static, Gate>) -> Held {
        g.held = true;
        Held
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        gate().held = false;
        FREED.notify_all();
    }
}

static RO_VOLUME: std::sync::OnceLock<RoVolume> = std::sync::OnceLock::new();

struct RoVolume {
    #[allow(dead_code)]
    pdrv: u8,
    #[allow(dead_code)]
    base: CString,
    drive: CString,
    fs: *mut FATFS,
}

unsafe impl Send for RoVolume {}
unsafe impl Sync for RoVolume {}

pub fn register_read_window(sd: &SdCard) -> Result<(), EspError> {
    if RO_VOLUME.get().is_some() {
        return Ok(());
    }

    let mut pdrv: u8 = 0;
    esp!(unsafe { ff_diskio_get_drive(&mut pdrv) })?;

    let drive = CString::new(format!("{pdrv}:")).expect("no interior NUL");
    let base = CString::new(RO_BASE_PATH).expect("no interior NUL");

    if let Err(e) = esp!(unsafe { beamer_fat_ro_register(pdrv, sd.raw()) }) {
        unsafe { ff_diskio_release(pdrv) };
        return Err(e);
    }

    let mut fs: *mut FATFS = core::ptr::null_mut();
    if let Err(e) =
        esp!(unsafe { esp_vfs_fat_register(base.as_ptr(), drive.as_ptr(), MAX_FILES, &mut fs) })
    {
        unsafe { ff_diskio_release(pdrv) };
        return Err(e);
    }

    let _ = RO_VOLUME.set(RoVolume {
        pdrv,
        base,
        drive,
        fs,
    });
    log::info!("read window registered on drive {pdrv} at {RO_BASE_PATH}");
    Ok(())
}

pub struct ReadWindow {
    _held: Held,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct OpenTiming {
    pub lock_us: u32,
    pub mount_us: u32,
}

impl ReadWindow {
    /// blocks until there's an open window
    pub fn open_next(
        sd: &SdCard,
        still_wanted: impl Fn() -> bool,
    ) -> Result<Option<ReadWindow>, EspError> {
        let _ = sd;
        let mut g = gate();
        g.scan_waiting = true;
        while g.held {
            g = match FREED.wait_timeout(g, SCAN_POLL) {
                Ok((g, _)) => g,
                Err(e) => e.into_inner().0,
            };
            if !still_wanted() {
                g.scan_waiting = false;
                drop(g);
                FREED.notify_all();
                return Ok(None);
            }
        }
        g.scan_waiting = false;
        ReadWindow::mount(Held::claim(g)).map(Some)
    }

    /// blocks as long as max_wait, then gives up
    pub fn open_measured(
        sd: &SdCard,
        max_wait: Duration,
    ) -> Result<Option<(ReadWindow, OpenTiming)>, EspError> {
        let _ = sd;
        let t0 = unsafe { esp_idf_svc::sys::esp_timer_get_time() };
        let g = match FREED.wait_timeout_while(gate(), max_wait, |g| g.held || g.scan_waiting) {
            Ok((g, _)) => g,
            Err(e) => e.into_inner().0,
        };
        if g.held || g.scan_waiting {
            return Ok(None);
        }
        let held = Held::claim(g);
        let t1 = unsafe { esp_idf_svc::sys::esp_timer_get_time() };
        let window = ReadWindow::mount(held)?;
        let t2 = unsafe { esp_idf_svc::sys::esp_timer_get_time() };
        Ok(Some((
            window,
            OpenTiming {
                lock_us: (t1 - t0) as u32,
                mount_us: (t2 - t1) as u32,
            },
        )))
    }

    /// doesn't block at all
    pub fn try_open(sd: &SdCard) -> Result<Option<ReadWindow>, EspError> {
        let _ = sd;
        let g = gate();
        if g.held || g.scan_waiting {
            return Ok(None);
        }
        ReadWindow::mount(Held::claim(g)).map(Some)
    }

    pub fn path(&self, rel: &str) -> String {
        format!("{RO_BASE_PATH}/{rel}")
    }

    /// `rel` as FatFs itself names it (`1:/SLIPPI/x.slp`), for
    /// [`for_each_entry`] and [`stat`].
    pub fn fat_path(&self, rel: &str) -> CString {
        match RO_VOLUME.get() {
            Some(vol) => fat_path(&vol.drive, rel),
            None => CString::new(rel).expect("no interior NUL"),
        }
    }

    /// The FAT's place and size, as this mount read them.
    pub fn geometry(&self) -> Option<FatGeometry> {
        let vol = RO_VOLUME.get()?;
        // SAFETY: the window holds the mount
        let fs = unsafe { &*vol.fs };
        Some(FatGeometry {
            fs_type: fs.fs_type,
            fat_base: fs.fatbase,
            fat_sectors: fs.fsize,
            entries: fs.n_fatent,
            cluster_sectors: fs.csize as u32,
        })
    }

    fn mount(held: Held) -> Result<ReadWindow, EspError> {
        let Some(vol) = RO_VOLUME.get() else {
            log::error!("read window used before it was registered");
            return Err(EspError::from_infallible::<
                { esp_idf_svc::sys::ESP_ERR_INVALID_STATE },
            >());
        };

        let res = unsafe { f_mount(vol.fs, vol.drive.as_ptr(), 1) };
        if res != 0 {
            log::error!("f_mount({RO_BASE_PATH}) failed: FRESULT {res}");
            return Err(EspError::from_infallible::<
                { esp_idf_svc::sys::ESP_ERR_NOT_FOUND },
            >());
        }

        Ok(ReadWindow { _held: held })
    }

    pub fn for_each_replay(
        &self,
        cap: u32,
        mut f: impl FnMut(&str),
    ) -> std::io::Result<(u32, bool)> {
        let mut n = 0u32;
        for entry in std::fs::read_dir(self.path("SLIPPI"))? {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    log::warn!("SLIPPI/: skipping an unreadable entry: {e}");
                    continue;
                }
            };
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !crate::publish::is_replay_name(name) {
                continue;
            }
            if n >= cap {
                return Ok((cap, true));
            }
            n += 1;
            f(name);
        }
        Ok((n, false))
    }
}

impl Drop for ReadWindow {
    fn drop(&mut self) {
        if let Some(vol) = RO_VOLUME.get() {
            unsafe { f_mount(core::ptr::null_mut(), vol.drive.as_ptr(), 0) };
        }
    }
}
