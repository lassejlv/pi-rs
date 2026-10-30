// @earendil-works/pi-durable/package.json: the upstream manifest's public fields (pi-mono 5e63241).
export default {
  "name": "@earendil-works/pi-durable",
  "version": "1.4.0",
  "description": "Durable conversation, task, and document runtime for Pi",
  "type": "module",
  "main": "./dist/index.js",
  "types": "./dist/index.d.ts",
  "exports": {
    ".": {
      "source": "./src/index.ts",
      "types": "./dist/index.d.ts",
      "import": "./dist/index.js",
      "default": "./dist/index.js"
    },
    "./env": {
      "source": "./src/env/index.ts",
      "types": "./dist/env/index.d.ts",
      "import": "./dist/env/index.js",
      "default": "./dist/env/index.js"
    },
    "./env/node": {
      "source": "./src/env/node.ts",
      "types": "./dist/env/node.d.ts",
      "import": "./dist/env/node.js",
      "default": "./dist/env/node.js"
    },
    "./codemode": {
      "source": "./src/codemode/index.ts",
      "types": "./dist/codemode/index.d.ts",
      "import": "./dist/codemode/index.js",
      "default": "./dist/codemode/index.js"
    },
    "./tool-search": {
      "source": "./src/tool-search/index.ts",
      "types": "./dist/tool-search/index.d.ts",
      "import": "./dist/tool-search/index.js",
      "default": "./dist/tool-search/index.js"
    },
    "./tools": {
      "source": "./src/tools/index.ts",
      "types": "./dist/tools/index.d.ts",
      "import": "./dist/tools/index.js",
      "default": "./dist/tools/index.js"
    },
    "./images": {
      "source": "./src/images/index.ts",
      "types": "./dist/images/index.d.ts",
      "import": "./dist/images/index.js",
      "default": "./dist/images/index.js"
    },
    "./images/node": {
      "source": "./src/images/node.ts",
      "types": "./dist/images/node.d.ts",
      "import": "./dist/images/node.js",
      "default": "./dist/images/node.js"
    },
    "./images/cloudflare": {
      "source": "./src/images/cloudflare.ts",
      "types": "./dist/images/cloudflare.d.ts",
      "import": "./dist/images/cloudflare.js",
      "default": "./dist/images/cloudflare.js"
    },
    "./storage/memory": {
      "source": "./src/storage/memory.ts",
      "types": "./dist/storage/memory.d.ts",
      "import": "./dist/storage/memory.js",
      "default": "./dist/storage/memory.js"
    },
    "./storage/indexeddb": {
      "source": "./src/storage/indexeddb.ts",
      "types": "./dist/storage/indexeddb.d.ts",
      "import": "./dist/storage/indexeddb.js",
      "default": "./dist/storage/indexeddb.js"
    },
    "./storage/jsonl": {
      "source": "./src/storage/jsonl/index.ts",
      "types": "./dist/storage/jsonl/index.d.ts",
      "import": "./dist/storage/jsonl/index.js",
      "default": "./dist/storage/jsonl/index.js"
    },
    "./storage/jsonl/node": {
      "source": "./src/storage/jsonl/node.ts",
      "types": "./dist/storage/jsonl/node.d.ts",
      "import": "./dist/storage/jsonl/node.js",
      "default": "./dist/storage/jsonl/node.js"
    },
    "./storage/sqlite": {
      "source": "./src/storage/sqlite/index.ts",
      "types": "./dist/storage/sqlite/index.d.ts",
      "import": "./dist/storage/sqlite/index.js",
      "default": "./dist/storage/sqlite/index.js"
    },
    "./storage/sqlite/cloudflare": {
      "source": "./src/storage/sqlite/cloudflare.ts",
      "types": "./dist/storage/sqlite/cloudflare.d.ts",
      "import": "./dist/storage/sqlite/cloudflare.js",
      "default": "./dist/storage/sqlite/cloudflare.js"
    },
    "./storage/sqlite/node": {
      "source": "./src/storage/sqlite/node.ts",
      "types": "./dist/storage/sqlite/node.d.ts",
      "import": "./dist/storage/sqlite/node.js",
      "default": "./dist/storage/sqlite/node.js"
    },
    "./testing": {
      "source": "./src/testing/index.ts",
      "types": "./dist/testing/index.d.ts",
      "import": "./dist/testing/index.js",
      "default": "./dist/testing/index.js"
    },
    "./package.json": "./package.json"
  },
  "sideEffects": false,
  "keywords": [
    "durable-execution",
    "agents",
    "conversations",
    "tasks"
  ],
  "author": "Earendil Works",
  "license": "MIT",
  "repository": {
    "type": "git",
    "url": "git+https://github.com/earendil-works/pi.git",
    "directory": "packages/durable"
  },
  "engines": {
    "node": ">=22.19.0"
  },
  "dependencies": {
    "@earendil-works/chord": "1.4.0",
    "@earendil-works/pi-ai": "1.4.0",
    "@earendil-works/pi-codemode": "1.4.0",
    "@earendil-works/pi-telemetry": "1.4.0",
    "@silvia-odwyer/photon": "0.3.3",
    "diff": "8.0.4",
    "typebox": "1.3.27"
  }
};
