#!/bin/sh
# z3rm demo welcome — runs on first serial login

clear
printf '\033[1;36m'
cat <<'BANNER'
     _____ _____                 
    |__  /|___ / _ __ _ __ ___  
      / /   |_ \| '__| '_ ` _ \ 
     / /_ ___) | |  | | | | | |
    /____|____/|_|  |_| |_| |_|

BANNER
printf '\033[0m'
printf '\033[1mYour shells outlive the window.\033[0m\n\n'
printf 'This is a real Linux VM running in your browser.\n'
printf 'The terminal you see is rendered by Z3rm'\''s GPUI engine\n'
printf 'through the same mux protocol used in production.\n\n'
printf '\033[90m── try it ──────────────────────────────────────\033[0m\n\n'
printf '  \033[33muname -a\033[0m          kernel info\n'
printf '  \033[33mcat /proc/cpuinfo\033[0m CPU details\n'
printf '  \033[33mfree -h\033[0m           memory usage\n'
printf '  \033[33mls /mnt/\033[0m          9p shared filesystem\n\n'
printf '\033[90m────────────────────────────────────────────────\033[0m\n\n'
exec /bin/sh
