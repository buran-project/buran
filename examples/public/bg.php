<?php
echo "instant response\n";
fastcgi_finish_request();
sleep(2);
file_put_contents("/tmp/buran-bg-marker", "background done at " . microtime(true) . "\n");
