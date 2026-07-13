# Laravel on Buran: current-generation framework e2e. The skeleton comes
# from `composer create-project` at image build time (layer-cached), with
# composer running inside the target image so dependencies resolve against
# its exact PHP version. Laravel's post-create scripts generate the app key,
# create the sqlite database and run the migrations.

ARG BURAN_IMAGE=buran:php-8.4
FROM ${BURAN_IMAGE}

COPY --from=composer:2 /usr/bin/composer /usr/local/bin/composer

RUN apt-get update \
    && apt-get install --no-install-recommends -y git unzip \
    && rm -rf /var/lib/apt/lists/*

RUN composer create-project --prefer-dist --no-progress laravel/laravel /www

# Native route on top of the welcome page: a large streamed response that
# exercises Buran's chunked write path.
COPY routes-web.php /www/routes/web.php
