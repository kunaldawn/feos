//! t3-fs: std::fs on feos.
//!
//!     t3-fs DIR            files, metadata, directories, rename, removal, symbolic
//!                          links, locks and times under DIR (made, then removed)
//!     t3-fs --links DIR    hard links and permissions, which need a filesystem that
//!                          keeps them (a native mount; the RAM root has neither)
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::os::feos::fs::MetadataExt as FeosMetadataExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::time::{Duration, SystemTime};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--links") => links(Path::new(&args[2])),
        Some(dir) => general(Path::new(dir)),
        None => panic!("usage: t3-fs DIR | t3-fs --links DIR"),
    }
}

fn general(dir: &Path) {
    let _ = fs::remove_dir_all(dir);
    fs::create_dir_all(dir.join("a/b/c")).unwrap();
    assert!(dir.join("a/b/c").is_dir());

    // Create, write, read.
    let file = dir.join("file.txt");
    fs::write(&file, b"hello feos\n").unwrap();
    assert_eq!(fs::read_to_string(&file).unwrap(), "hello feos\n");
    let mut f = OpenOptions::new().append(true).open(&file).unwrap();
    f.write_all(b"second line\n").unwrap();
    drop(f);
    assert_eq!(fs::read_to_string(&file).unwrap(), "hello feos\nsecond line\n");
    let mut f = OpenOptions::new().read(true).write(true).open(&file).unwrap();
    f.seek(SeekFrom::Start(6)).unwrap();
    f.write_all(b"FEOS").unwrap();
    f.seek(SeekFrom::Start(0)).unwrap();
    let mut s = String::new();
    f.read_to_string(&mut s).unwrap();
    assert_eq!(s, "hello FEOS\nsecond line\n");
    f.set_len(5).unwrap();
    f.sync_all().unwrap();
    f.sync_data().unwrap();
    drop(f);
    assert_eq!(fs::read(&file).unwrap(), b"hello");
    let e = OpenOptions::new().write(true).create_new(true).open(&file).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::AlreadyExists);
    let e = File::open(dir.join("missing")).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::NotFound);
    println!("files: ok");

    // Metadata.
    let m = fs::metadata(&file).unwrap();
    assert!(m.is_file() && !m.is_dir() && !m.is_symlink());
    assert_eq!(m.len(), 5);
    assert_eq!(m.size(), 5);
    assert_eq!(m.st_size(), 5);
    assert!(m.ino() != 0 && m.ino() == m.st_ino());
    assert!(m.nlink() >= 1);
    let modified = m.modified().unwrap();
    let age = SystemTime::now().duration_since(modified).unwrap_or_default();
    assert!(age < Duration::from_secs(300), "mtime is {age:?} old");
    assert!(m.accessed().is_ok());
    assert!(fs::metadata(dir).unwrap().is_dir());
    println!("metadata: len={} ino={} nlink={} mode={:o} uid={}", m.len(), m.ino(), m.nlink(), m.mode(), m.uid());

    // Times.
    let when = SystemTime::UNIX_EPOCH + Duration::new(1_700_000_000, 123_456_789);
    File::options().write(true).open(&file).unwrap()
        .set_times(fs::FileTimes::new().set_modified(when)).unwrap();
    let got = fs::metadata(&file).unwrap().modified().unwrap();
    assert_eq!(got.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs(), 1_700_000_000);
    File::open(&file).unwrap().set_modified(SystemTime::now()).unwrap();
    println!("times: ok");

    // Directories.
    for name in ["x1", "x2", "x3"] {
        fs::write(dir.join("a").join(name), name).unwrap();
    }
    let mut names: Vec<(String, bool)> = fs::read_dir(dir.join("a")).unwrap()
        .map(|e| {
            let e = e.unwrap();
            (e.file_name().into_string().unwrap(), e.file_type().unwrap().is_dir())
        })
        .collect();
    names.sort();
    assert_eq!(names, [("b".into(), true), ("x1".into(), false), ("x2".into(), false), ("x3".into(), false)]);
    let entry = fs::read_dir(dir.join("a")).unwrap().map(Result::unwrap)
        .find(|e| e.file_name() == "x2").unwrap();
    assert_eq!(entry.metadata().unwrap().len(), 2);
    assert_eq!(entry.path(), dir.join("a/x2"));
    let e = fs::remove_dir(dir.join("a")).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::DirectoryNotEmpty);
    println!("read_dir: {:?}", names.iter().map(|n| n.0.as_str()).collect::<Vec<_>>());

    // Rename, copy, removal.
    fs::rename(dir.join("a/x1"), dir.join("moved")).unwrap();
    assert!(!dir.join("a/x1").exists() && dir.join("moved").exists());
    fs::rename(dir.join("moved"), dir.join("a/x2")).unwrap(); // replaces x2
    assert_eq!(fs::read_to_string(dir.join("a/x2")).unwrap(), "x1");
    assert_eq!(fs::copy(dir.join("a/x3"), dir.join("copy")).unwrap(), 2);
    assert_eq!(fs::read_to_string(dir.join("copy")).unwrap(), "x3");
    fs::remove_file(dir.join("copy")).unwrap();
    assert!(!dir.join("copy").exists());
    println!("rename: ok");

    // Symbolic links.
    std::os::unix::fs::symlink("file.txt", dir.join("link")).unwrap();
    assert_eq!(fs::read_link(dir.join("link")).unwrap(), Path::new("file.txt"));
    assert!(fs::symlink_metadata(dir.join("link")).unwrap().is_symlink());
    assert_eq!(fs::metadata(dir.join("link")).unwrap().len(), 5);
    assert_eq!(fs::read(dir.join("link")).unwrap(), b"hello");
    assert_eq!(fs::canonicalize(dir.join("link")).unwrap(), fs::canonicalize(&file).unwrap());
    fs::remove_file(dir.join("link")).unwrap();
    assert!(file.exists());
    println!("symlink: ok");

    // Locks (std's File::lock is flock).
    let a = File::open(&file).unwrap();
    let b = File::open(&file).unwrap();
    a.lock().unwrap();
    assert!(b.try_lock_shared().is_err(), "an exclusive lock didn't hold");
    a.unlock().unwrap();
    b.try_lock_shared().unwrap();
    a.try_lock_shared().unwrap();
    assert!(File::open(&file).unwrap().try_lock().is_err());
    drop((a, b));
    println!("locks: ok");

    // Permissions where the filesystem keeps them: a read-only file can't be written.
    let mut p = fs::metadata(&file).unwrap().permissions();
    println!("permissions: mode {:o} readonly {}", p.mode(), p.readonly());
    p.set_mode(0o644);
    let _ = fs::set_permissions(&file, p);

    fs::remove_dir_all(dir).unwrap();
    assert!(!dir.exists());
    println!("remove_dir_all: ok");
    println!("t3-fs: ok");
}

fn links(dir: &Path) {
    let _ = fs::remove_dir_all(dir);
    fs::create_dir_all(dir).unwrap();
    let file = dir.join("file");
    fs::write(&file, b"linked").unwrap();
    fs::hard_link(&file, dir.join("hard")).unwrap();
    let a = fs::metadata(&file).unwrap();
    let b = fs::metadata(dir.join("hard")).unwrap();
    assert_eq!(a.ino(), b.ino());
    assert_eq!(a.nlink(), 2);
    assert_eq!(fs::read(dir.join("hard")).unwrap(), b"linked");
    fs::remove_file(&file).unwrap();
    assert_eq!(fs::metadata(dir.join("hard")).unwrap().nlink(), 1);
    println!("hard_link: ino={} nlink=2 then 1", b.ino());

    // A hard link to a symbolic link links the link itself (linkat without
    // AT_SYMLINK_FOLLOW).
    std::os::unix::fs::symlink("hard", dir.join("sym")).unwrap();
    fs::hard_link(dir.join("sym"), dir.join("sym2")).unwrap();
    assert!(fs::symlink_metadata(dir.join("sym2")).unwrap().is_symlink());
    println!("hard_link of a symlink: ok");

    let target = dir.join("hard");
    fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o600);
    let mut p = fs::metadata(&target).unwrap().permissions();
    p.set_readonly(true);
    fs::set_permissions(&target, p).unwrap();
    assert!(fs::metadata(&target).unwrap().permissions().readonly());
    assert_eq!(fs::metadata(&target).unwrap().mode() & 0o777, 0o400);
    fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(fs::metadata(&target).unwrap().mode() & 0o777, 0o755);
    println!("permissions: 600, read-only 400, 755");
    fs::remove_dir_all(dir).unwrap();
    println!("t3-fs --links: ok");
}
