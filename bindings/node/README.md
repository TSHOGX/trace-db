# TraceDB Node.js binding

This package is a thin napi-rs wrapper around the public Rust `TraceDb`
facade. Archive behavior stays in Rust; Node methods return JSON strings to
keep the native surface small and stable.

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
const rows = JSON.parse(db.searchJson("deploy", 10));
console.log(rows);
```

The addon uses N-API 6 and supports Node.js 18 or newer. Rust 1.82 or newer is
required to build from source.
