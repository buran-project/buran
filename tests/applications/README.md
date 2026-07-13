# Application e2e tests

Real-world applications running end to end through Buran: each subdirectory
is a self-contained docker-compose stack with a `run.sh` runner that
brings it up, exercises it over HTTP and tears it down (`KEEP=1` to leave
the stack running for debugging). `./run.sh` at this level runs them all
(no fail-fast, full report); pass names to run a subset.

These are the top of the test pyramid — slow and coarse, but every app
runs on every PR (`tests.yml`, one runner per app): legacy compatibility is
the product, so it gets gated, not sampled. They answer one
question: does a real framework still work on Buran? The per-version
build-and-serve check for every matrix cell (legacy included) lives in
`tests/smoke/` and gates every PR — see `.github/workflows/tests.yml`.

The apps deliberately spread across the PHP matrix — modern frameworks on
fresh branches, legacy on old ones (the lifeboat positioning):

| App | PHP | Covers |
|---|---|---|
| wordpress | 8.4 | front controller, pretty permalinks, REST API, static assets, cookie/POST login flow, wp-cli install (spec 2.13 MVP exit criterion), APCu shared cache + opcache (mu-plugin probe) |
| laravel | 8.4 | welcome page, framework 404, static assets, session cookie over the SAPI, large streamed (chunked) response |
| symfony | 8.5 | attribute-routed controller (JSON), dev welcome page, router 404, runtime version pin, request-body round-trip (POST + raw `php://input`) |
| yii3 | 8.3 | front page, framework 404, runtime version pin |
| yii2 | 7.4 | front page, query routing (`?r=`), static assets, framework 404, runtime version pin, `fastcgi_finish_request` flush-then-log deferred work |

Beyond "does the framework serve a page", each app carries a couple of native
PHP-runtime probes — expressed through the framework's own routing (a
controller/action, a Laravel route, a WordPress mu-plugin) rather than a bare
script — so the interesting slice of the SAPI contract is asserted where it
actually matters: version pinning, request bodies, chunked streaming, APCu
shared memory, opcache, and post-response deferred work.

Framework skeletons come from `composer create-project` at image build time
(layer-cached; composer runs inside the target image so dependencies
resolve against its exact PHP version). WordPress unpacks from the official
image and installs via wp-cli baked into the test image.
