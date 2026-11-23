//! t3-args-env: arguments from relibc's System V start, and the environment.
//!
//!     T3_ENV=from-shell t3-args-env one "two three"
use std::env;
use std::ffi::OsString;
use std::path::Path;

fn main() {
    let args: Vec<String> = env::args().collect();
    assert_eq!(args.len(), 3, "args: {args:?}");
    assert!(args[0].ends_with("t3-args-env"), "argv[0] = {}", args[0]);
    assert_eq!(args[1], "one");
    assert_eq!(args[2], "two three");
    let os: Vec<OsString> = env::args_os().collect();
    assert_eq!(os[2], OsString::from("two three"));
    println!("args: {:?}", &args[1..]);

    // The shell's `VAR=value cmd` reaches the program through SPAWN2's environment.
    assert_eq!(env::var("T3_ENV").as_deref(), Ok("from-shell"));
    assert!(env::vars().any(|(k, v)| k == "T3_ENV" && v == "from-shell"));
    assert!(env::var("T3_NOT_SET").is_err());
    unsafe { env::set_var("T3_SET", "value") };
    assert_eq!(env::var("T3_SET").as_deref(), Ok("value"));
    unsafe { env::remove_var("T3_SET") };
    assert!(env::var_os("T3_SET").is_none());
    println!("env: T3_ENV={}", env::var("T3_ENV").unwrap());

    // /proc/self/exe.
    let exe = env::current_exe().unwrap();
    assert_eq!(exe, Path::new("/bin/t3-args-env"), "current_exe");
    println!("current_exe: {}", exe.display());

    let cwd = env::current_dir().unwrap();
    assert!(cwd.is_absolute());
    env::set_current_dir("/tmp").unwrap();
    assert_eq!(env::current_dir().unwrap(), Path::new("/tmp"));
    env::set_current_dir(&cwd).unwrap();
    assert_eq!(env::temp_dir(), Path::new("/tmp"));
    println!("cwd: {}", cwd.display());

    assert_eq!(env::consts::OS, "feos");
    assert_eq!(env::consts::FAMILY, "unix");
    assert!(cfg!(target_os = "feos") && cfg!(unix));
    println!("os: {} {} {}", env::consts::OS, env::consts::FAMILY, env::consts::ARCH);
    println!("t3-args-env: ok");
}
