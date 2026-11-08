//! Built-in programs and text that seed the RAM drive.
//!
//! These are real `.COM` images, assembled by hand, so `DEMO` exercises
//! the CPU, the DOS interrupt path and the text adapter end to end.

/// A `.COM` program that prints a banner and returns to the prompt.
pub const HELLO_COM: &[u8] = &[
    0xBA, 0x0C, 0x01, // MOV DX, 010Ch (message)
    0xB4, 0x09, // MOV AH, 09h (print string)
    0xCD, 0x21, // INT 21h
    0xB8, 0x00, 0x4C, // MOV AX, 4C00h (terminate)
    0xCD, 0x21, // INT 21h
    b'H', b'e', b'l', b'l', b'o', b' ', b'f', b'r', b'o', b'm', b' ', b'F', b'E', b'O', b'S', b'-',
    b'D', b'O', b'S', b'!', b'\r', b'\n', b'$',
];

/// A `.COM` program that draws a small CP437 box and a message.
pub const BOX_COM: &[u8] = &[
    0xBA, 0x0C, 0x01, // MOV DX, 010Ch
    0xB4, 0x09, // MOV AH, 09h
    0xCD, 0x21, // INT 21h
    0xB8, 0x00, 0x4C, // MOV AX, 4C00h
    0xCD, 0x21, // INT 21h
    // Each row is 12 cells: a corner or side, 10 inside, and the other one.
    0xC9, 0xCD, 0xCD, 0xCD, 0xCD, 0xCD, 0xCD, 0xCD, 0xCD, 0xCD, 0xCD, 0xBB, b'\r', b'\n', //
    0xBA, b' ', b'F', b'E', b'O', b'S', b'-', b'D', b'O', b'S', b' ', 0xBA, b'\r', b'\n', //
    0xC8, 0xCD, 0xCD, 0xCD, 0xCD, 0xCD, 0xCD, 0xCD, 0xCD, 0xCD, 0xCD, 0xBC, b'\r', b'\n', //
    b'$',
];

/// `README.TXT` contents.
pub const README: &[u8] = b"FEOS-DOS 1.0\r\n\
\r\n\
This is an x86 virtual machine and DOS prompt built into feos.\r\n\
It has its own writable C: RAM drive, so files you create here last until\r\n\
the DOS window closes.\r\n\
\r\n\
Commands: DIR CD CLS ECHO TYPE COPY DEL REN MD RD VER MEM VOL DATE TIME\r\n\
          MOUNT GAMES SCREEN HELP ABOUT DEMO RUN EXIT\r\n\
\r\n\
MOUNT WOLF3D imports a game from the board's FEOS filesystem into C:\\WOLF3D.\r\n\
GAMES lists the shipped game image. SCREEN writes C:\\SCREEN.BMP.\r\n\
\r\n\
DEMO runs a real .COM program through the emulated CPU.\r\n\
Type HELP for more.\r\n";

/// `AUTOEXEC.BAT` contents.
pub const AUTOEXEC: &[u8] = b"@ECHO OFF\r\nPROMPT $P$G\r\nECHO Welcome to FEOS-DOS.\r\n";
