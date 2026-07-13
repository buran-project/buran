<?php

namespace App\Controller;

use Symfony\Component\HttpFoundation\JsonResponse;
use Symfony\Component\Routing\Attribute\Route;

final class HelloController
{
    #[Route('/hello')]
    public function __invoke(): JsonResponse
    {
        return new JsonResponse(['hello' => 'buran']);
    }
}
