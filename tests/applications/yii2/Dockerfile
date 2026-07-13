# Yii2 on Buran: the legacy-framework e2e, deliberately on PHP 7.4 — the
# lifeboat positioning in action. The skeleton comes from `composer
# create-project` at build time (layer-cached), with composer running inside
# the target image so dependencies resolve against PHP 7.4. Yii's
# post-create hook fills in the cookie validation key.

ARG BURAN_IMAGE=buran:php-7.4-debian
FROM ${BURAN_IMAGE}

COPY --from=composer:2 /usr/bin/composer /usr/local/bin/composer

RUN apt-get update \
    && apt-get install --no-install-recommends -y git unzip \
    && rm -rf /var/lib/apt/lists/*

RUN composer create-project --prefer-dist --no-progress yiisoft/yii2-app-basic /www

# Native controller with the PHP-runtime probes (version pin + the
# flush-then-log deferred-work pattern via fastcgi_finish_request).
COPY ProbeController.php /www/controllers/ProbeController.php
