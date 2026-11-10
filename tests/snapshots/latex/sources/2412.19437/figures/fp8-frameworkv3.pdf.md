Input

BF16

Input

Gradient To FP8

To BF16

Fprop

Σ

FP32

Weight

Σ

FP32

Dgrad To BF16

To FP8

To FP8

Output

Output

Gradient

BF16

To FP8

Wgrad

Σ

FP32

To FP8

Master

Weight To FP32

Weight

Gradient

FP32

Optimizer

States To BF16
