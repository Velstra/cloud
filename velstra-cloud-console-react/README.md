# Cloud console

React/TypeScript frontend for the Cloud API. Requires Node.js 22.12+ and npm.

From the Cloud repository root, start the disposable API fixture:

```sh
FAKE_PORT=18300 node velstra-cloud-console/tests/console/fake-api.mjs
```

In another terminal:

```sh
cd velstra-cloud-console-react
npm ci
npm run dev -- --host 127.0.0.1
```

Open [localhost:5180](http://localhost:5180). Fixture login: `operator`,
password `a test operator passphrase`. This is synthetic data, not a real cell.

To use a development cell instead, set `VELSTRA_API` to its API URL when
starting Vite. The proxy accepts self-signed TLS for local development; do not
expose it as a production gateway.

## Check

From this directory:

```sh
npm run lint
npm run build
npm run test:browser
```

Browser tests require Chromium/Chrome and create their own API fixtures.
Set `CHROMIUM` if needed. [Full testing guide](../docs/testing.md).
