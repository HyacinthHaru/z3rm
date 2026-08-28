#!/bin/sh
# Rebuild the 9p filesystem served to the v86 guest: the static i686-musl
# mux_server, the no-dependency landing TUI, and their startup wrapper.
set -e
cd "$(dirname "$0")"
OUT=../../public/v86/fs
mkdir -p "$OUT"
: "${RUSTFLAGS:=-C linker=rust-lld -C strip=symbols -C panic=abort}"
export RUSTFLAGS
cargo build -p z3rm_guest_tui --target i686-unknown-linux-musl --release
cargo build -p mux_server --manifest-path ../../../crates/mux_server/Cargo.toml \
  --target i686-unknown-linux-musl --no-default-features --features guest --release
STAGE=$(mktemp -d)
cp ../../../target/i686-unknown-linux-musl/release/z3rm-tui "$STAGE/z3rm-tui"
cp ../../public/media/z3rm-terminal-grid.png "$STAGE/z3rm-terminal-grid.png"
cat > "$STAGE/z3rm" <<'SCRIPT'
#!/bin/sh
case "${1-}" in
  a|attach|landing)
    exec /mnt/z3rm-tui
    ;;
  *)
    printf '%s\n' 'usage: /mnt/z3rm {a|attach|landing}' >&2
    exit 2
    ;;
esac
SCRIPT
cp ../../../target/i686-unknown-linux-musl/release/z3rm-server "$STAGE/mux_server"
rm -f ../../public/v86/z3rm-server ../../public/v86/z3rm-server.bin
cp "$STAGE/mux_server" ../../public/v86/z3rm-server.bin
cat > "$STAGE/start-mux.sh" <<'SCRIPT'
#!/bin/sh
mkdir -p /dev/pts
mount -t devpts devpts /dev/pts 2>/dev/null || true
dmesg -n 1 2>/dev/null
stty -F /dev/ttyS0 raw -echo 2>/dev/null
printf 'Z3RM_MUX_READY'
export PATH=/mnt:$PATH
exec /mnt/mux_server --serial /dev/ttyS0
SCRIPT
chmod +x "$STAGE/start-mux.sh" "$STAGE/mux_server" "$STAGE/z3rm-tui" "$STAGE/z3rm"
python3 - "$OUT" <<'PY'
import os, sys
for filename in os.listdir(sys.argv[1]):
    if filename.endswith(".bin"):
        os.remove(os.path.join(sys.argv[1], filename))
PY
python3 tools/fs2json.py --out "$OUT/fs.json" "$STAGE"
python3 - "$STAGE" "$OUT" <<'PY'
import hashlib, os, sys
stage, out = sys.argv[1], sys.argv[2]
for f in os.listdir(stage):
    h = hashlib.sha256()
    with open(os.path.join(stage, f), "rb", buffering=0) as fh:
        for b in iter(lambda: fh.read(128*1024), b""):
            h.update(b)
    data = open(os.path.join(stage, f), "rb").read()
    open(os.path.join(out, h.hexdigest()[:8] + ".bin"), "wb").write(data)
PY
rm -rf "$STAGE"
echo "guest fs packaged into $OUT"
