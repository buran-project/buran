<?php

header('Content-Type: application/json');
$s = $_SERVER;
ksort($s);
echo json_encode($s, JSON_UNESCAPED_SLASHES | JSON_PRETTY_PRINT), "\n";
