<?php
// DEV/DEMO ONLY — leaks runtime internals (opcache state, memory_limit, PID).
// Do not ship this in a production docroot; remove it or gate it behind a
// trusted source. Not copied into the official images.
$s = opcache_get_status(false);
echo "opcache_enabled=" . var_export($s["opcache_enabled"] ?? false, true) . "\n";
echo "memory_limit=" . ini_get("memory_limit") . "\n";
echo "pid=" . getmypid() . "\n";
