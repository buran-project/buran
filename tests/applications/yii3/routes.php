<?php

declare(strict_types=1);

use App\Web;
use Yiisoft\Router\Group;
use Yiisoft\Router\Route;

return [
    Group::create()
        ->routes(
            Route::get('/')
                ->action(Web\HomePage\Action::class)
                ->name('home'),
            // Native PHP-runtime probe alongside the home route.
            Route::get('/probe')
                ->action(Web\Probe\Action::class)
                ->name('probe'),
        ),
];
