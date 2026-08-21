# TraceDB Python binding

This package is a thin PyO3 wrapper around the public Rust `TraceDb` facade.
The archive, parser, search, and reconstruction behavior stays in Rust. The
public Python facade returns native dictionaries and lists; matching `_json`
methods retain access to the small raw native surface.

Build locally with [maturin](https://www.maturin.rs/):

```bash
cd bindings/python
python -m pip install maturin
maturin develop --release
```

Example:

```python
from tracedb import TraceDb

db = TraceDb.open()
rows = db.search("deploy", limit=10)
print(rows)
```

Available operations are `ingest`, `list`, `search`, `show`, `stats`, `reindex`,
and `reconstruct`. Paths accept strings and `os.PathLike` objects.

The extension uses the stable Python 3.10 ABI and requires Rust 1.83 or newer.
