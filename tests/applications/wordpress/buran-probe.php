<?php
/*
 * Plugin Name: Buran Probe
 * Description: Exercises APCu shared memory and opcache through the Buran SAPI.
 *
 * A real WordPress deployment leans on APCu (persistent object cache, shared
 * across a worker pool) and opcache; a must-use plugin is WordPress's native
 * "always loaded, no activation" extension point, so it is the honest place
 * to expose a probe. Hit /?buran_probe=1 — a second hit must report a higher
 * counter, proving the shared segment survives across requests and workers.
 */

add_action('muplugins_loaded', static function (): void {
    if (!isset($_GET['buran_probe'])) {
        return;
    }

    $hits = function_exists('apcu_inc') ? apcu_inc('buran_probe_hits') : 0;
    $apcu = function_exists('apcu_enabled') && apcu_enabled();
    $opcache = function_exists('opcache_get_status')
        && (opcache_get_status(false)['opcache_enabled'] ?? false);

    header('Content-Type: text/plain');
    printf(
        "php=%s pid=%d apcu=%s hits=%d opcache=%s\n",
        PHP_VERSION,
        getmypid(),
        $apcu ? 'on' : 'off',
        (int) $hits,
        $opcache ? 'on' : 'off'
    );
    exit;
});
