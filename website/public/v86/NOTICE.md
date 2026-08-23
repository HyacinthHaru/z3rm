# Third-party runtime assets

This directory vendors the runtime assets required to boot a Linux guest in the
browser with [v86](https://github.com/copy/v86). They are committed so the
static Pages deploy serves them without a build-time fetch.

## v86 (libv86.js, v86.wasm, v86-fallback.wasm)

- Source: <https://github.com/copy/v86> (npm package `v86@0.5.441`)
- Copyright (c) 2012-2024 Fabian Hemmer and v86 contributors
- License: BSD 2-Clause "Simplified" License

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are met:

1. Redistributions of source code must retain the above copyright notice, this
   list of conditions and the following disclaimer.
2. Redistributions in binary form must reproduce the above copyright notice,
   this list of conditions and the following disclaimer in the documentation
   and/or other materials provided with the distribution.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND
ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED
WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

## SeaBIOS (seabios.bin)

- Source: <https://www.seabios.org/> (via <https://github.com/copy/v86> `bios/`)
- License: GNU Lesser General Public License, version 3 (LGPLv3)
- See <https://www.gnu.org/licenses/lgpl-3.0.html>

## buildroot-bzimage.bin

- Source: <https://i.copy.sh/buildroot-bzimage.bin> (v86 demo image)
- A 32-bit Linux kernel bzImage built with [Buildroot](https://buildroot.org/),
  shipping a BusyBox userland.
  - Linux kernel: GNU General Public License, version 2 (GPLv2)
    (<https://www.gnu.org/licenses/old-licenses/gpl-2.0.html>)
  - BusyBox: GNU General Public License, version 2 (GPLv2)
  - Buildroot itself: GNU General Public License, version 2 or later

Kernel and Buildroot configuration and sources are available from the upstream
projects; this image is redistributed unmodified as published by the v86
project for demonstration purposes.
