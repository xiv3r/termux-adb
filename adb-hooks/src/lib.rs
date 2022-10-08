#![feature(c_variadic)]

#[macro_use]
extern crate lazy_static;

use std::{
    env,
    ffi::{CString, CStr, OsStr},
    fs::File,
    io::Write,
    mem,
    os::raw::{c_char, c_int},
    sync::Mutex, collections::HashMap, path::PathBuf, ptr::null_mut,
};

use std::os::unix::ffi::OsStrExt;

use libc::{DIR, dirent, O_CREAT, mode_t, DT_CHR, DT_DIR};

use rand::Rng;

use redhook::{
    hook, real, real2,
};

use nix::unistd::{lseek, Whence};

lazy_static! {
    static ref LOG: Mutex<File> = Mutex::new(File::create("./tadb-log.txt").unwrap());
}

const BASE_DIR_ORIG: &str = "/dev/bus/usb";
const BASE_DIR_REMAPPED: &str = "./fakedev/bus/usb";

macro_rules! log {
    ($($arg:tt)*) => {
        _ = writeln!(&mut *LOG.lock().unwrap(), $($arg)*);
    };
}

fn to_string(s: &CStr) -> String {
    // OsStr::from_bytes(s.to_bytes()).to_owned()
    match s.to_str() {
        Ok(s) => s.to_owned(),
        Err(e) => e.to_string(),
    }
}

fn to_cstr(b: &[c_char]) -> &CStr {
    unsafe { CStr::from_ptr(b.as_ptr()) }
}

// our directory structure will always be flat
// so we can have just one dirent per DirStream
struct DirStream {
    pos: i32,
    entry: dirent,
}

trait NameSetter {
    fn set_name(&mut self, name: &OsStr);
}

impl NameSetter for dirent {
    fn set_name(&mut self, name: &OsStr) {
        for (i, j) in self.d_name.iter_mut().zip(
            name.as_bytes().iter().chain([0].iter())
        ) {
            *i = *j as i8;
        }
    }
}

fn dirent_new(off: i64, typ: u8, name: &OsStr) -> dirent {
    let mut rng = rand::thread_rng();
    let mut entry = dirent {
        d_ino: rng.gen(),
        d_off: off,
        d_reclen: mem::size_of::<dirent>() as u16,
        d_type: typ,
        d_name: [0; 256],
    };
    entry.set_name(name);

    entry
}

lazy_static! {
    static ref DIR_MAP: HashMap<PathBuf, DirStream> = {
        let mut dir_map = HashMap::new();
        if let Ok(usb_dev_path) = env::var("TERMUX_USB_DEV").map(|str| PathBuf::from(str)) {
            if let Some(usb_dev_name) = usb_dev_path.file_name() {
                let mut last_entry = dirent_new(
                    0, DT_CHR, usb_dev_name
                );
                let mut current_dir = usb_dev_path;

                while current_dir.pop() {
                    dir_map.insert(current_dir.clone(), DirStream{
                        pos: 0,
                        entry: last_entry.clone(),
                    });
                    last_entry = dirent_new(
                        0, DT_DIR, current_dir.as_os_str()
                    );

                    if current_dir.as_os_str() == BASE_DIR_ORIG {
                        break;
                    }
                }
            }
        }
        dir_map
    };
}

enum HookedDir {
    Native(*mut DIR),
    Virtual(DirStream)
}

hook! {
    unsafe fn opendir(name: *const c_char) -> *mut DIR => tadb_opendir {
        let dir_name = to_string(CStr::from_ptr(name));
        let remapped_name = dir_name.replacen(BASE_DIR_ORIG, BASE_DIR_REMAPPED, 1);
        let remapped_name_c = CString::new(remapped_name.as_str()).unwrap();

        log!("[TADB] called opendir with {}, remapping to {}", &dir_name, &remapped_name);

        let dir = real!(opendir)(remapped_name_c.as_ptr());
        let result = Box::new(HookedDir::Native(dir));
        Box::into_raw(result) as *mut DIR
    }
}

hook! {
    unsafe fn closedir(dirp: *mut DIR) -> c_int => tadb_closedir {
        let hooked_dir = Box::from_raw(dirp as *mut HookedDir);
        match hooked_dir.as_ref() {
            &HookedDir::Native(dirp) => real!(closedir)(dirp),
            &HookedDir::Virtual(ref _dirstream) => 0 // TODO
        }
    }
}

hook! {
    unsafe fn readdir(dirp: *mut DIR) -> *mut dirent => tadb_readdir {
        let hooked_dir = &*(dirp as *mut HookedDir);

        match hooked_dir {
            &HookedDir::Native(dirp) => {
                log!("[TADB] called readdir with native DIR* {:?}", dirp);
                let result = real!(readdir)(dirp);
                if let Some(r) = result.as_ref() {
                    log!("[TADB] readdir returned dirent with d_name={}", to_string(to_cstr(&r.d_name)));
                }
                result
            }
            &HookedDir::Virtual(ref _dirstream) => {
                null_mut() // TODO
            }
        }
    }
}

hook! {
    unsafe fn close(fd: c_int) -> c_int => tadb_close {
        if let Ok(usb_fd_str) = env::var("TERMUX_USB_FD") {
            if let Ok(usb_fd) = usb_fd_str.parse::<c_int>() {
                // usb fd must not be closed
                if usb_fd == fd {
                    return 0;
                }
            }
        }
        real!(close)(fd)
    }
}

type OpenFn = unsafe extern "C" fn(*const c_char, c_int, ...) -> c_int;

#[no_mangle]
pub unsafe extern "C" fn open(pathname: *const c_char, flags: c_int, mut args: ...) -> c_int {
    // let real_open = real2!(open);
    // There is some problem with caching the real function value (TODO: fix)
    let real_open: OpenFn = mem::transmute(redhook::ld_preload::dlsym_next("open\0"));

    let name = to_string(CStr::from_ptr(pathname));
    // eprintln!("DEBUG name: {}", name);
    // prevent infinite recursion when logfile is first initialized
    if name != "./tadb-log.txt" {
        log!("[TADB] called open with pathname={} flags={}", name, flags);
    }

    if name.starts_with(BASE_DIR_ORIG) {
        if let Ok(usb_fd_str) = env::var("TERMUX_USB_FD") {
            if let Ok(usb_fd) = usb_fd_str.parse::<c_int>() {
                if let Err(e) = lseek(usb_fd, 0, Whence::SeekSet) {
                    log!("[TADB] error seeking fd {}: {}", usb_fd, e);
                }
                log!("[TADB] open hook returning fd with value {}", usb_fd);
                return usb_fd;
            }
        }
    }

    let result = if (flags & O_CREAT) == 0 {
        real_open(pathname, flags)
    } else {
        real_open(pathname, flags, args.arg::<mode_t>())
    };

    // prevent infinite recursion when logfile is first initialized
    if name != "./tadb-log.txt" {
        log!("[TADB] open returned fd with value {}", result);
    }
    result
}