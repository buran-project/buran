# WordPress on Buran: what a real deployment adds on top of the stock image.
#
# The buran:php-* images ship bare PHP (plus opcache) by design; anything the
# application needs comes in exactly like with the official php images:
#   - Debian flavor: docker-php-ext-install / pecl (below)
#   - Alpine flavor: apk packages, e.g.
#       FROM buran:php-8.4-alpine
#       RUN apk add --no-cache php84-mysqli php84-pecl-apcu
#
# WordPress needs mysqli to talk to MariaDB; APCu backs a persistent object
# cache (shared per worker pool, survives requests).

ARG BURAN_IMAGE=buran:php-8.4
FROM ${BURAN_IMAGE}

RUN docker-php-ext-install -j"$(nproc)" mysqli \
    && pecl install apcu \
    && docker-php-ext-enable apcu

# Test tooling, not part of the extension pattern above: run.sh execs
# `wp` right in this container (the image ships full php-cli anyway).
ADD --chmod=755 https://raw.githubusercontent.com/wp-cli/builds/gh-pages/phar/wp-cli.phar /usr/local/bin/wp
