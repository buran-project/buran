<?php
/* Minimal wp-config for the Buran demo stack (docker-compose.yml). */

define('DB_NAME', 'wordpress');
define('DB_USER', 'wordpress');
define('DB_PASSWORD', 'wordpress');
define('DB_HOST', 'db');
define('DB_CHARSET', 'utf8mb4');
define('DB_COLLATE', '');

/* Fixed dev-only keys: this stack is a local demo, not a deployment. */
define('AUTH_KEY',         'buran-demo-key-auth');
define('SECURE_AUTH_KEY',  'buran-demo-key-secure-auth');
define('LOGGED_IN_KEY',    'buran-demo-key-logged-in');
define('NONCE_KEY',        'buran-demo-key-nonce');
define('AUTH_SALT',        'buran-demo-salt-auth');
define('SECURE_AUTH_SALT', 'buran-demo-salt-secure-auth');
define('LOGGED_IN_SALT',   'buran-demo-salt-logged-in');
define('NONCE_SALT',       'buran-demo-salt-nonce');

$table_prefix = 'wp_';

define('WP_DEBUG', false);

if (!defined('ABSPATH')) {
    define('ABSPATH', __DIR__ . '/');
}

require_once ABSPATH . 'wp-settings.php';
