#!/bin/sh
# Replace the console shell with the mux server: serial0 becomes the mux
# protocol transport; shells are PTY children of the server inside the guest.
dmesg -n 1 2>/dev/null
stty -F /dev/ttyS0 raw -echo 2>/dev/null
exec /mnt/mux_server --serial /dev/ttyS0
