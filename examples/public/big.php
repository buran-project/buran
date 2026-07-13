<?php
// stream ~200KiB to force chunked path
for ($i = 0; $i < 200; $i++) { echo str_repeat("x", 1024); }
echo "\nEND-MARKER\n";
