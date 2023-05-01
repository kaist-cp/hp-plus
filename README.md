# HP++: A backward-compatible extension for Hazard Pointers

This is an implementation of *HP++*, a safe memory reclamation scheme, which is proposed in

> Jaehwang Jung, Janggun Lee, Jeonghyeon Kim and Jeehoon Kang, Applying Hazard Pointers to More Concurrent Data Structures, SPAA 2023.

The benchmark suite which evaluates the performance of HP++ can be found at [smr-benchmark](https://github.com/kaist-cp/smr-benchmark) repository.

## Introduction

[*Hazard pointers (HP)*](https://dl.acm.org/doi/10.1145/571825.571829), a popular and one of the earliest reclamation schemes, require each thread to announce the *protection* of each object before accessing. But this alone doesn't guarantee safety, because another thread may have concurrently retired and reclaimed the same object. To ensure safety, *validation* is needed to check whether the pointer is freed or not. However, it's impossible to precisely decide, so validation often *over-approximates* the freed pointer set. HP is not applicable to many high-performance data structures that *optimistically traverse* nodes. Their traversal follows the link to the next node even when the next node might have been detached from the data structure. This is fundamentally incompatible with HP’s validation by over-approximating unreachability. To use HP, the data structure must forgo *optimistic traversal*, resulting in significantly lower performance compared to structures using *optimistic traversal*.

We design **HP++, a backward-compatible extension for HP** to improve applicability by enabling optimistic traversal. The key idea is *under-approximating* the unreachability in validation to allow optimistic traversal by letting the deleter mark the node *after* detaching, and *patching up* the potentially unsafe accesses arising from false-negatives by letting the deleter protect such pointers. Thanks to optimistic traversal, data structures with HP++ outperform the same-purpose data structures with HP under contention while consuming a similar amount of memory.

## API Design

You can check the actual implementation of [Harris's list](https://www.cl.cam.ac.uk/research/srg/netos/papers/2001-caslists.pdf) in [tests/harris_list.rs](tests/harris_list.rs).

This crate provides two major APIs: `try_protect_pp` and `try_unlink`, corresponding to **TryProtect** and **TryUnlink** in the original paper, respectively.

(`.._pp`, which stands for *plus-plus*, is used in `try_protect_pp` to distinguish with `try_protect` function that provides HP version of protecting.)

### `try_protect_pp`

```rust
pub fn try_protect_pp<T, S, F>(
    &mut self,
    ptr: *mut T,
    src: &S,
    src_link: &AtomicPtr<T>,
    check_stop: &F,
) -> Result<(), ProtectError<T>>
where
    F: Fn(&S) -> bool,
```

Traversing threads use `try_protect_pp` to protect a pointer loaded from a source object.

It takes 4 arguments (except the `HazardPointer` to protect with):

1. `ptr`: the pointer to protect.
2. `src`: the reference of the source object.
3. `src_link`: the field of `src` from which `ptr` was loaded.
4. `is_invalid`: the predicate to check whether src is invalidated.

If `src` is invalidated, `try_protect_pp` returns `false` meaning that it is unsafe to create new protection from `src`. Otherwise, it returns `true`, but if `src_link` has changed from `ptr`, then the new value is written to `ptr`.

### `try_unlink`

```rust
pub unsafe fn try_unlink<T>(
    unlink: impl Unlink<T>,
    frontier: &[*mut T]
) -> bool
where
    T: Invalidate,
```

Unlinking threads use `try_unlink` to physically delete and retire node(s) while protecting the traversing threads. The protection will persist until the retired nodes are invalidated by reclaimers.

It takes 2 arguments:

1. `unlink`: the Trait object which implements `do_unlink` function that performs unlinking and returns the unlinked node(s).
2. `frontier`: the pointers that the unlinker has to protect for the traversing threads.

Note that the node type `T` implements the `Invalidate` trait, so that unlinked nodes can be later invalidated by the reclaimers.

Invalidation can be implemented by adding a flag to the node. But in most cases, this can be done without extra space overhead using tagged pointers, similar to logical deletion.

`try_unlink` returns whether the unlink was successful.