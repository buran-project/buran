<?php
$n = apcu_inc("hits", 1, $ok);
if ($n === false) { apcu_add("hits", 1); $n = 1; }
echo "pid=" . getmypid() . " hits=" . $n . " apcu=" . var_export(apcu_enabled(), true) . "\n";
