# Symfony on Buran: current-generation framework e2e on the newest PHP
# branch. The skeleton comes from `composer create-project` at build time
# (layer-cached); attribute-routed controllers are added on top so there are
# real routes to exercise besides the dev welcome page — a JSON endpoint plus
# PHP-runtime probes (version pin, request-body round-trip).

ARG BURAN_IMAGE=buran:php-8.5
FROM ${BURAN_IMAGE}

COPY --from=composer:2 /usr/bin/composer /usr/local/bin/composer

RUN apt-get update \
    && apt-get install --no-install-recommends -y git unzip \
    && rm -rf /var/lib/apt/lists/*

RUN composer create-project --prefer-dist --no-progress symfony/skeleton /www

COPY HelloController.php /www/src/Controller/HelloController.php
COPY ProbeController.php /www/src/Controller/ProbeController.php
