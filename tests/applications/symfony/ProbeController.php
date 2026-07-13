<?php

namespace App\Controller;

use Symfony\Component\HttpFoundation\JsonResponse;
use Symfony\Component\HttpFoundation\Request;
use Symfony\Component\Routing\Attribute\Route;

/**
 * PHP-runtime probes exercised through Symfony's native routing — the parts
 * of the SAPI contract the demo pages don't touch.
 */
final class ProbeController
{
    /**
     * Pin the runtime version: proves the php85 module really executes PHP
     * 8.5, not that some string happened to render.
     */
    #[Route('/probe', methods: ['GET'])]
    public function probe(): JsonResponse
    {
        return new JsonResponse(['php' => PHP_VERSION]);
    }

    /**
     * Request-body round-trip: a form field plus the raw php://input length
     * and hash, so both the parsed POST and the raw stream are asserted.
     */
    #[Route('/echo', methods: ['POST'])]
    public function echo(Request $request): JsonResponse
    {
        $raw = $request->getContent();

        return new JsonResponse([
            'a'   => $request->request->get('a'),
            'len' => strlen($raw),
            'md5' => md5($raw),
        ]);
    }
}
