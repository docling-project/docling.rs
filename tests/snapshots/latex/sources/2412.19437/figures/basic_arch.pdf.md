Transformer Block ×𝐿𝐿

Feed-Forward Network

RMSNorm

Attention

RMSNorm DeepSeekMoE

0

... ...

Output Hidden 𝐡𝐡𝑡𝑡′

1

...

𝑁𝑁𝑠𝑠

1

2

Router

3

4

Top-𝐾𝐾𝑟𝑟

... ...

1

Input Hidden 𝐮𝐮𝑡𝑡

Multi-Head Latent Attention (MLA)

0

{[𝐪𝐪𝑡𝑡,𝑖𝑖

{𝐪𝐪𝑡𝑡,𝑖𝑖

𝐶𝐶 }

Output Hidden 𝐮𝐮𝑡𝑡

Cached During Inference

... ...

Multi-Head Attention

𝐶𝐶 ; 𝐪𝐪𝑡𝑡,𝑖𝑖 𝑅𝑅 ]}

apply

RoPE

{[𝐤𝐤𝑡𝑡,𝑖𝑖

concatenate

𝑅𝑅 }

𝐤𝐤𝑡𝑡𝑅𝑅

{𝐪𝐪𝑡𝑡,𝑖𝑖

...

Latent 𝐜𝐜𝑡𝑡𝑄𝑄

apply

RoPE

𝐶𝐶 ; 𝐤𝐤𝑡𝑡𝑅𝑅]}

concatenate

𝐶𝐶 }

{𝐤𝐤𝑡𝑡,𝑖𝑖

Latent 𝐜𝐜𝑡𝑡𝐾𝐾𝐾𝐾

𝐶𝐶 }

{𝐯𝐯𝑡𝑡,𝑖𝑖

...

Input Hidden 𝐡𝐡𝑡𝑡

... ...

...

Routed Expert

Shared Expert

𝑁𝑁𝑟𝑟-1 𝑁𝑁𝑟𝑟
