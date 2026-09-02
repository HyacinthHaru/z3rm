---
title: Terminal images
description: What the Kitty graphics support covers, and how images behave when a pane scrolls or resizes.
translationKey: reference-terminal-images
section: reference
order: 5
status: verified
---

# Terminal images

A program running in a pane can draw images by writing the Kitty graphics
protocol to its terminal. `mux_server` recognises the sequence before the text
reaches the emulator, so the image never lands in the grid as stray bytes, and
publishes it as a typed notification the client renders.

## What is supported

Transmission carries a base64 PNG payload, split across as many `m=1` chunks
as the program needs. The server reassembles them in the order they arrived and
publishes one image when the final chunk lands. `a=T` is the action to send;
anything that is not a delete is treated as a transmission.

| Key | Meaning |
| --- | --- |
| `a` | Action. `d` deletes; anything else transmits. |
| `f` | Format. `100` (PNG), or omitted. The client refuses anything else. |
| `i` | Image id, used to replace or delete an image later. |
| `c`, `r` | Size in terminal cells. |
| `m` | More chunks follow. |
| `d` | Delete selector. |

`q` is accepted and ignored: the payload is always base64. `x` and `y` are crop
offsets rather than cell coordinates, so they are ignored too — placement comes
from the cursor cell at the time the sequence arrived.

Anything else in the sequence is ignored rather than refused. An unrecognised
key should cost the program nothing, and a malformed sequence is logged and
dropped without disturbing the surrounding text.

## Scrolling

Images are anchored to the live viewport. Scrolling back into history hides
them until the viewport returns to the bottom.

This is deliberate. The image is not part of the scrollback the server keeps —
that is a grid of cells — so there is no historical row it belongs to. Painting
it at the same screen position over older text would put it somewhere it never
was.

## Resizing

A placement keeps the cell coordinates it was given. Resizing a pane reflows
the text around it; the image does not move and is not rescaled. A program that
wants its image to follow a resize redraws it, which is the same thing it does
for any other output it cares about.

## Limits

Each pane holds at most 256 images and 256 MB of decoded frames. A single
placement may be at most 4096 cells on a side. A frame that would exceed a
limit is refused and the images already on screen stay: a stream of otherwise
valid frames should not be able to evict what a program already drew, nor
exhaust the browser process.

## Availability

The same path serves both clients. The desktop app and the WebAssembly client
render from the same notification, because the client that draws the image is
the same code in both.
