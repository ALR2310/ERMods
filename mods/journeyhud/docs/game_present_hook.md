# Overlay hook on the game's own Present function

Status: **Confirmed** in game on 2.7.2.0 with ERSS 4.14.1, 5.1.0 and 6.0.0 (overlay draws, game stays up).
A resolution change, HDR and three third-party mods (map and wheel-menu mods, one of them an overlay mod that hooks DXGI) were tried by the user and work, and so was a long play session on Convergence with ME2. ME3 was tried by the user with ERSS 4.14.1, 5.1.0 and 6.0.0 and works. Not checked: window-mode change. Conventions:
[research notes](../../../docs/research/README.md). Code: `vendor/hudhook/src/hooks/dx12.rs`
(`ImguiDx12GameHooks`), `shared/src/overlay/mod.rs` (`apply_dx12_hooks`, `locate_game_present`).

## Question

hudhook hooks DXGI (a dummy swap chain gives the addresses). ERSS wraps both `CreateSwapChain` and
`CreateSwapChainForComposition`, so the dummy swap chain is ERSS's wrapper (see
[swap_chain_wrappers.md](swap_chain_wrappers.md)), and ERSS 5.1.0 crashed in its own code when the dummy swap
chain was made (`ERSS.dll+0x28DCC8`, a null COM `Release`). Can the overlay avoid DXGI altogether?

## Observations

- The game calls `Present` from one function, `sub_141EA0D20` (RVA `0x1EA0D20` on 2.7.2.0): `a1` is the game's
  swap chain object whose first field is the `IDXGISwapChain*`; the call is `(*a1)->Present(a3, 0)` (vtable slot
  8, offset `0x40`), then it signals the fence on the game's queue.
- The swap chain is made by `sub_141EA01B0` with `IDXGIFactory::CreateSwapChain(queue, desc, a1)`; the queue is
  `*(state + 8)` where `state` is returned by `sub_141E8BE00` (`lea rax, [global]; ret`, global at RVA
  `0x4808340`). The same queue state is read by the present function through the call at `+0x5A`.
- The function's first 0x70 bytes with wildcards on the three call targets are unique on 2.7.0.0, 2.7.1.0 and
  2.7.2.0 (`scripts/ida/run.ps1 aob -Version all`).

## Fix

A MinHook detour on the game's present function (the pattern in `GAME_PRESENT_PATTERN`). Before calling the
original it renders on `*a1` with the queue from the state getter (the getter's address is the target of the
`call` at `+0x5A`, so no RVA is stored). The existing `render` of hudhook is reused unchanged. Nothing in
`dxgi.dll`, no vtable and no swap chain wrapper is touched; mods that wrap or patch DXGI are not in the path.
When the pattern is not found the old DXGI hooks are used and the log says so.

## Open

- The game's resize path was not found (the swap chain may be recreated); the overlay resets its pipeline when the
  swap chain or its device changes. A resolution change works; a window-mode change was not tried.
- HDR `Auto` still reads the colour space through the `SetColorSpace1` vtable slot patch; whether it follows ERSS's
  HDR switch was not looked at (the user's HDR test worked, the mode used was not noted).
- Another mod that hooks the same game function (the pattern then fails and the DXGI path is used).
