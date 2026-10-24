# vyges-dft

Scan insertion and stitching, so a chip can be tested after fabrication.

> **Status: in development.** Every command currently refuses with exit 3. Commands are enabled one
> at a time as each is validated.

## Why this exists

A commercial chip has to be testable once it is made. Scan chains turn every flip-flop into a point a tester can set and read.

## Usage

```
vyges physical dft run <design.odb> <job> -o OUT.odb
vyges physical dft report <design.odb> <job>
vyges physical dft --describe
```

## Install

```
vyges install physical
```

## License

Apache-2.0. See `LICENSE` and `NOTICE`.
