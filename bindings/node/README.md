# TraceDB Node.js binding

This package is a thin napi-rs wrapper around the public Rust `TraceDb`
facade. Archive behavior stays in Rust; the public JavaScript facade returns
native objects while matching `*Json` methods retain the small raw native
surface. TypeScript declarations ship with the package.

Build and test locally without npm dependencies:

```bash
cd bindings/node
npm run build
npm test
```

Example:

```javascript
const { TraceDb } = require("@tracedb/core");

const db = TraceDb.open();
const rows = db.search("deploy", { limit: 10 });
console.log(rows);
```

Available operations are `ingest`, `list`, `search`, `show`, `stats`, `reindex`,
and `reconstruct`.

The addon uses N-API 6 and supports Node.js 18 or newer. Rust 1.83 or newer is
required to build from source.
