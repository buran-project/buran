<?php

use Illuminate\Support\Facades\Route;
use Symfony\Component\HttpFoundation\StreamedResponse;

Route::get('/', function () {
    return view('welcome');
});

// Large streamed body (~200 KiB): drives Buran's chunked write path end to
// end, the way a framework streaming an export or a long render would.
Route::get('/big', function () {
    return new StreamedResponse(function () {
        for ($i = 0; $i < 200; $i++) {
            echo str_repeat('x', 1024);
        }
        echo "\nEND-MARKER\n";
    }, 200, ['Content-Type' => 'text/plain']);
});
