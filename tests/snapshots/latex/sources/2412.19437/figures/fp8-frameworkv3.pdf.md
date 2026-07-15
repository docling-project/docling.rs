Input

BF16

Input

Gradient To FP8

To BF16

Σ

FP32

Output

Output

Gradient

BF16

<!-- image -->

Fprop

Σ

FP32

Weight

Dgrad To BF16

To FP8

To FP8

To FP8

To FP8

Wgrad

Σ

FP32

Master

Weight To FP32

Weight

Gradient

FP32

Optimizer

States To BF16
