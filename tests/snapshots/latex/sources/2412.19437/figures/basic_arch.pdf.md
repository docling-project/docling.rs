Transformer Block ×𝐿𝐿

Feed-Forward Network

RMSNorm

Attention

RMSNorm

0

Output Hidden 𝐡𝐡𝑡𝑡′

1

...

𝑁𝑁𝑠𝑠

... ...

1

2

Router

1 ... ...

3

4

Top-𝐾𝐾𝑟𝑟

Multi-Head Latent Attention (MLA)

...

Routed Expert

Shared Expert

𝑁𝑁𝑟𝑟-1 𝑁𝑁𝑟𝑟

Input Hidden 𝐮𝐮𝑡𝑡

0

Output Hidden 𝐮𝐮𝑡𝑡

concatenate

{𝐪𝐪𝑡𝑡,𝑖𝑖 𝐶𝐶 } {𝐪𝐪𝑡𝑡,𝑖𝑖 𝑅𝑅 }

...

{[𝐪𝐪𝑡𝑡,𝑖𝑖 𝐶𝐶 ; 𝐪𝐪𝑡𝑡,𝑖𝑖 𝑅𝑅 ]}

Input Hidden

... ...

Cached During Inference

Multi-Head Attention 𝐤𝐤𝑡𝑡𝑅𝑅

concatenate

Latent 𝐜𝐜𝑡𝑡𝐾𝐾𝐾𝐾

𝐶𝐶 } {𝐤𝐤𝑡𝑡,𝑖𝑖 𝐶𝐶 }

{𝐯𝐯𝑡𝑡,𝑖𝑖

...

{[𝐤𝐤𝑡𝑡,𝑖𝑖 𝐶𝐶 ; 𝐤𝐤𝑡𝑡𝑅𝑅]}

apply

RoPE

... ...

apply

Latent 𝐜𝐜𝑡𝑡𝑄𝑄 RoPE
