# Codex Proxy
<img width="491" height="325" alt="image" src="https://github.com/user-attachments/assets/543059ad-149a-4fce-9a9a-fd389b7f6760" />
<img width="525" height="322" alt="image" src="https://github.com/user-attachments/assets/cd22d004-5dc8-4d2c-9073-252b604b1acc" />

## Compile

```bash
cd codex-proxy
cargo build --release
```

The executable will be here:

```bash
./target/release/codex-proxy
```

## macOS

```bash
cargo build --release
./target/release/codex-proxy
```

## Windows

Run in PowerShell:

```powershell
cargo build --release
.\target\release\codex-proxy.exe
```

## Linux ARM

On the Linux ARM server:

```bash
cargo build --release
./target/release/codex-proxy
```

For Oracle ARM Ubuntu, this is the normal build path.

## Quick Check

```bash
./target/release/codex-proxy --help
```
