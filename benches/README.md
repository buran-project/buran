# Buran benchmark suite

Reproducible, container-based comparison of three identical PHP stacks:

| Stack | Port | What it is |
|---|---|---|
| buran | 8081 | Buran built from this repo (`docker/php-debian.Dockerfile`) |
| freeunit | 8082 | `ghcr.io/freeunitorg/freeunit:latest-php8.4` |
| nginx+fpm | 8083 | `nginx:stable` + `php:8.4-fpm-trixie` |

All three run PHP 8.4 from the same `php:8.4-*-trixie` image lineage,
8 static workers each, serving the same `app/index.php`.

## Requirements

- Linux (host networking is used on purpose: docker-proxy would dominate
  the measurement)
- docker + compose plugin
- [`oha`](https://github.com/hatoo/oha) — optional; falls back to
  `ghcr.io/hatoo/oha` container

## Run

```console
$ ./run.sh              # 10s per stack, 128 connections
$ ./run.sh 30s 256      # longer & heavier
$ docker compose down   # teardown
```

The script builds the Buran image on first run, waits for readiness,
verifies all three endpoints return the same body, warms each stack up,
then measures RPS / p50 / p99 / success rate.

## Fairness notes

- same base image lineage, same PHP version, same worker count, same script;
- access logs disabled everywhere (Buran: not configured, nginx/fpm: off);
- opcache state is whatever the base images ship — identical across stacks;
- host networking everywhere, warmup run before each measurement.
