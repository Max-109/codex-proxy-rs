# Codex Proxy

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
