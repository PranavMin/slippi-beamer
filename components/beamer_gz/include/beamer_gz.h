#pragma once

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C"
{
#endif

   int beamer_gz_begin(int level);

   int beamer_gz_push(const uint8_t *in, size_t in_len, size_t *in_used,
                      uint8_t *out, size_t out_cap, size_t *out_len,
                      int finish);

   void beamer_gz_end(void);

   size_t beamer_gz_arena_size(void);
   size_t beamer_gz_arena_high_water(void);

/* Hands the arena to the heap for good, for a station that never gzips
 * (LazyTO mode); beamer_gz_begin fails from then on. False when it could
 * not (a stream is open, or the heap refused the region). */
bool beamer_gz_donate_arena(void);

#ifdef __cplusplus
}
#endif
