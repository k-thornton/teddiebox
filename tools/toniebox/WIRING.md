# FTDI → Toniebox wiring (ESP32 board rev 1.6.C)

```
 FTDI SH-U09C5                         Toniebox J103 (3 bare pads)
 voltage jumper on 3V3                 near D100/R149, just below J102

   TXD ──────────────┐   ┌──────────── TxD  (pad 1)
                     ╳                       
   RXD ──────────────┘   └──────────── RxD  (pad 2, middle)
   GND ─────────────────────────────── GND  (pad 3)
   VCC   not connected (the box runs on its own battery/charger)
```

TX and RX cross over: adapter TXD → box RxD, adapter RXD → box TxD.
Read the adapter's silkscreen for its own pin order. Check the box board's
printed revision before trusting this pad order.

## Download mode (J100)

J100 is the ESP32's GPIO0 (boot) jumper; find its `J100` label on the board.
Short it, then power the box on from fully off. The chip only checks J100
at power-on, so let go once it's powered. A reset does not count; pull power.

## Checks

- **Port:** `ls /dev/cu.usbserial-*`, then `export PORT=/dev/cu.usbserial-…`
  in `~/git/teddybox/.envrc.local`.
- **Wiring:** serial monitor at 115200, normal power-on. Boot text means the
  wiring is right; unreadable characters mean TX/RX are swapped. Close the
  monitor afterward, since it blocks the flashing tools.
- **Download mode:** the ROM prints `boot:0x0 (DOWNLOAD(USB/UART0))` and
  `waiting for download`, LED off, no sound.
- **`BROWNOUT_RST`:** power problem; charge or plug in the box.
