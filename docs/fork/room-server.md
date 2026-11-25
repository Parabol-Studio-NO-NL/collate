# Run a Collate room in the studio

The room server keeps one document available when everyone quits Collate. Up to three desktop editors can join at once. Each connects to the host through Collate’s encrypted peer transport. The host keeps the initial document, embedded artwork, and confirmed edits and comments on disk. A display, Dropbox account and browser are unnecessary.

## Try it on one Mac or Linux machine

Build the server from the same Collate revision as the clients. The Linux CI workflow lists the Rust toolchain used to build Collate. On Linux, install the native development libraries used by the [Linux build](../../.github/workflows/ci.yml) before compiling. You can build on another machine with the same OS and architecture.

```sh
cargo build --locked --release -p collate-room-server
server=./target/release/collate-room-server
room="$HOME/Collate Studio Room"
"$server" init "$room" "/path/to/your-document.designcraft"
"$server" serve "$room"
```

Omit the document path to create a blank room. Import reads the original once. For linked artwork, first use **Embed** in Collate’s Links panel and save the document. Import removes the local source paths from the server’s copy. The room holds the working copy from then on.

The server initializes new rooms with protocol 17. Use an updated desktop build to join them. The same server opens existing protocol 15 room directories and serves them in their original protocol.

In another terminal, copy the contents of `invitation.txt` inside the room directory. On macOS, run `pbcopy < "$HOME/Collate Studio Room/invitation.txt"`. Choose **Join shared document…** on Collate’s Home screen and paste the invitation. Use a second Collate instance or another computer to test simultaneous edits. For a second macOS instance run `open -n -a Collate`. Anyone with the invitation can join and edit, so send it privately.

## Connect from another network

The default listener uses UDP port 7337. The same-network setup works directly on a LAN or a routed VPN. For a public VPS, allow that UDP port in its firewall and the provider firewall. Add `--relay` to `serve` to enable Iroh’s default public discovery and encrypted relay fallback when a direct path is unavailable. The relay forwards traffic while the room state stays on your server. A static IP or a routable VPN address keeps existing invitations useful after restarts.

Run one process per document with a separate private room directory. Set `COLLATE_ROOM_PORT` to another UDP port for each additional room. Keep the host awake and connected while editors need access. The server loads each room’s confirmed artwork into memory. On an Apple Silicon Mac, a blank room used about 20 MiB of resident memory. A test room with one 1 GiB PNG used about 1.02 GiB while listening and peaked at 2.51 GiB during native document import. The test image had small pixel dimensions. Larger decoded images and multiple rooms need additional headroom. Allow disk space for the room, incoming temporary files, exports and backups.

## Keep a room running on an office Mac

The launch agent runs in your user account after login and restarts the server if it exits unexpectedly. Stop the foreground server first. These commands use the room initialized above and the [launch agent template](../../packaging/room-server/studio.parabol.collate-room.plist).

```sh
mkdir -p "$HOME/Library/LaunchAgents" "$HOME/.local/bin"
install -m 0755 target/release/collate-room-server "$HOME/.local/bin/collate-room-server"
unit="$HOME/Library/LaunchAgents/studio.parabol.collate-room.plist"
cp packaging/room-server/studio.parabol.collate-room.plist "$unit"
/usr/libexec/PlistBuddy -c "Set :ProgramArguments:0 $HOME/.local/bin/collate-room-server" "$unit"
/usr/libexec/PlistBuddy -c "Set :ProgramArguments:2 $HOME/Collate Studio Room" "$unit"
launchctl bootstrap "gui/$(id -u)" "$unit"
```

For a maintenance stop, run `launchctl bootout "gui/$(id -u)" "$unit"`. Run `bootstrap` again when ready. For internet access with relay fallback, append `--relay` to the program arguments using `/usr/libexec/PlistBuddy -c 'Add :ProgramArguments:3 string --relay' "$unit"` before loading the unit.

## Keep a room running on a Linux VPS

From this checkout on a systemd-based Linux host, install the built binary, create a service account and initialize a room. To import an existing document, append a path readable by the service user to the `init` command.

```sh
sudo install -m 0755 target/release/collate-room-server /usr/local/bin/collate-room-server
sudo useradd --system --user-group --home-dir /var/lib/collate --shell /usr/sbin/nologin collate
sudo install -d -m 0700 -o collate -g collate /var/lib/collate
sudo -u collate /usr/local/bin/collate-room-server init /var/lib/collate/studio
sudo install -m 0644 packaging/room-server/collate-room.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now collate-room.service
sudo systemctl status collate-room.service
```

The [service unit](../../packaging/room-server/collate-room.service) enables relay fallback. Administrators can read the current invitation as the service user from `/var/lib/collate/studio/invitation.txt`. Logs show the file path. The invitation and document contents stay out of the logs.

## Restart, export and back up

Restart `serve` with the same room directory and port to recover the host identity, access token, original seed and confirmed revisions. A previous invitation works while one of its addresses remains reachable. When addresses change, fetch the current invitation file. To revoke invitations, stop the server, run `collate-room-server rotate-invite ROOM_DIR`, then start it and share the new invitation.

Back up the **whole room directory** with the server stopped or with a consistent filesystem snapshot. Room files are private to the owner, stored in plaintext, and include the host private key and invitation token. Use encrypted storage and backups where needed. Run a restored copy only after stopping the original host.

For a native document snapshot, stop the server and run `collate-room-server export ROOM_DIR NEW-FILE.designcraft`. The destination must be new. The snapshot contains the current layout and embedded artwork. Room comments, authorship and undo history stay in the room backup. Guests can also save a native document copy from Collate while connected.

Protocol 15 retains up to 1,024 revisions or 8 MiB of edit metadata per room. Protocol 17 retains up to 16,384 revisions or 64 MiB. Both account for artwork separately. When a room fills, export its document, keep its directory as the history archive and initialize a new room from the export. Distribute the new invitation to continue editing.
