<?php

namespace app\controllers;

use Yii;
use yii\web\Controller;
use yii\web\Response;

/**
 * PHP-runtime probes for the legacy lane, reachable via `?r=probe/<action>`.
 * These go through the SAPI rather than the framework's demo pages.
 */
class ProbeController extends Controller
{
    public $enableCsrfValidation = false;

    /** Marker the deferred job writes after its response is already flushed. */
    private const MARKER = '/tmp/buran-defer-marker';

    /**
     * Pin the runtime version: proves the php74 module really executes PHP
     * 7.4 — the whole point of the legacy lane.
     */
    public function actionInfo()
    {
        Yii::$app->response->format = Response::FORMAT_RAW;
        Yii::$app->response->headers->set('Content-Type', 'text/plain');

        return 'php=' . PHP_VERSION . "\n";
    }

    /**
     * The log-after-reply pattern every framework leans on: flush the reply,
     * close the FastCGI request so the client is served, then do the slow
     * "logging" work. A status action reads the marker back over HTTP, so the
     * test proves both the early flush and that the background work ran.
     */
    public function actionDefer()
    {
        @unlink(self::MARKER);

        $response = Yii::$app->response;
        $response->format = Response::FORMAT_RAW;
        $response->headers->set('Content-Type', 'text/plain');
        $response->content = "deferred\n";
        $response->send();

        if (function_exists('fastcgi_finish_request')) {
            fastcgi_finish_request();
        }

        sleep(2);
        file_put_contents(self::MARKER, (string) microtime(true));
    }

    /** Reports whether the deferred job has finished its post-response work. */
    public function actionStatus()
    {
        Yii::$app->response->format = Response::FORMAT_RAW;
        Yii::$app->response->headers->set('Content-Type', 'text/plain');

        return is_file(self::MARKER) ? "done\n" : "pending\n";
    }
}
