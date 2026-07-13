<?php

declare(strict_types=1);

namespace App\Web\Probe;

use Psr\Http\Message\ResponseFactoryInterface;
use Psr\Http\Message\ResponseInterface;

/**
 * PHP-runtime probe: reports the version through the SAPI so the e2e run can
 * pin that the php83 module really executes PHP 8.3, not just render a page.
 */
final readonly class Action
{
    public function __construct(
        private ResponseFactoryInterface $responseFactory,
    ) {}

    public function __invoke(): ResponseInterface
    {
        $response = $this->responseFactory->createResponse();
        $response->getBody()->write('php=' . PHP_VERSION . "\n");

        return $response->withHeader('Content-Type', 'text/plain');
    }
}
