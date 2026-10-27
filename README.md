# vyges-dft

Scan insertion and stitching, so a chip can be tested after fabrication.

> **Status: in development.** `replace`, `report` and `run` are built.

## Why this exists

A commercial chip has to be testable once it is made. Scan chains turn every flip-flop into a point a tester can set and read.

## Usage

```
vyges physical dft replace <design.odb> --liberty FILE... -o OUT.odb
vyges physical dft report  <design.odb> --liberty FILE... --sdc FILE [--max-length N] [--max-chains N]
                           [--clock-mixing no_mix|clock_mix]
vyges physical dft run     <design.odb> --liberty FILE... --sdc FILE -o OUT.odb [--max-length N]
                           [--max-chains N] [--clock-mixing no_mix|clock_mix]
vyges physical dft --describe
```

- `replace`: each flip-flop replaced by its closest scan equivalent in the library.
- `report`: the scan chains as they would be built, by clock domain and edge, within the length and
  count limits.
- `run`: the planned chains stitched (scan out to scan in, enables and ports added) and recorded in
  the database.

## Install

```
vyges install physical
```

It is part of the `vyges-physical` binary; `vyges physical dft --help` prints the full option list.

## License

Apache-2.0. See `LICENSE` and `NOTICE`.
