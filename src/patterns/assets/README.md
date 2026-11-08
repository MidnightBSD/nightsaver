# Pattern assets

`midnightbsd-logo.pbm` is a 1-bit (PBM P4, 240x200) mask of the MidnightBSD logo from
`/usr/src/stand/images/midnightbsd-logo.png` in the MidnightBSD source tree, regenerated with:

```
magick midnightbsd-logo.png -background white -flatten -colorspace gray -threshold 50% pbm:midnightbsd-logo.pbm
```

Set bits are logo pixels. It is embedded with `include_bytes!` by `src/patterns/midnight.rs`.
