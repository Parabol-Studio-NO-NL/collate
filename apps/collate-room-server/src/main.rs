//! Run one persistent Collate room from a local directory.
#[cfg(unix)]
fn run() -> Result<(), String> {
    use designcraft_ui_egui::room_server::{Server, export, init, rotate_invite};
    use std::{ffi::OsStr, path::Path};

    const USAGE: &str = "Usage:\n  collate-room-server init ROOM_DIR [DOCUMENT.designcraft]\n  collate-room-server serve ROOM_DIR [--relay]\n  collate-room-server export ROOM_DIR SNAPSHOT.designcraft\n  collate-room-server rotate-invite ROOM_DIR";
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let (command, room) = match args.as_slice() {
        [help] if help == "--help" || help == "-h" => {
            println!("{USAGE}");
            return Ok(());
        }
        [command, room, ..] => (command, Path::new(room)),
        _ => return Err(USAGE.into()),
    };
    match (command.to_str(), &args[2..]) {
        (Some("init"), []) => init(room, None),
        (Some("init"), [seed]) => init(room, Some(Path::new(seed))),
        (Some("rotate-invite"), []) => rotate_invite(room),
        (Some("export"), [output]) => export(room, Path::new(output)),
        (Some("serve"), options) if options.is_empty() || options == [OsStr::new("--relay")] => {
            let port = match std::env::var_os("COLLATE_ROOM_PORT") {
                Some(port) => port.to_str().and_then(|s| s.parse::<u16>().ok()).ok_or("Invalid COLLATE_ROOM_PORT.")?,
                None => 7337,
            };
            let mut server = Server::open(room, !options.is_empty(), port)?;
            eprintln!("Starting the room. The invitation will be written to {}.", room.join("invitation.txt").display());
            loop {
                std::thread::sleep(server.poll()?);
            }
        }
        _ => Err(USAGE.into()),
    }
}

fn main() -> std::process::ExitCode {
    #[cfg(unix)]
    let result = run();
    #[cfg(not(unix))]
    let result: Result<(), String> = Err("The room server requires Linux or macOS.".into());
    if let Err(error) = result {
        eprintln!("{error}");
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}
