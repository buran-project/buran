# Yii3 on Buran: the new generation of the Yii line. The app template comes
# from `composer create-project` at build time (layer-cached), with composer
# running inside the target image so dependencies resolve against its PHP.

ARG BURAN_IMAGE=buran:php-8.3
FROM ${BURAN_IMAGE}

COPY --from=composer:2 /usr/bin/composer /usr/local/bin/composer

RUN apt-get update \
    && apt-get install --no-install-recommends -y git unzip \
    && rm -rf /var/lib/apt/lists/*

RUN composer create-project --prefer-dist --no-progress yiisoft/app /www

# Native runtime probe: an extra action plus its route registration.
COPY Action.php /www/src/Web/Probe/Action.php
COPY routes.php /www/config/common/routes.php
