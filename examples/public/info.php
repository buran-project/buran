<?php
$s = opcache_get_status(false);
echo "opcache_enabled=" . var_export($s["opcache_enabled"] ?? false, true) . "\n";
echo "memory_limit=" . ini_get("memory_limit") . "\n";
echo "pid=" . getmypid() . "\n";
