# TraceDB Python binding

This package is a thin PyO3 wrapper around the public Rust `TraceDb` facade.
The archive, parser, search, and reconstruction behavior stays in Rust; Python
methods return JSON strings so the binding has a small, stable surface.

Build locally with [maturin](https://www.maturin.rs/):

```bash
cd bindings/python
python -m pip install maturin
maturin develop --release
```

Example:

```python
from tracedb import TraceDb
import json

db = TraceDb.open()
rows = json.loads(db.search_json("deploy", limit=10))
print(rows)
```

The extension uses the stable Python 3.10 ABI and requires Rust 1.82 or newer.
