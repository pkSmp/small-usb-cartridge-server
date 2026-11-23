// gzip on the card, both directions, as one work-item programs.
//
// This is the "mini CPU" half of `offload.rs`: deflate and inflate are
// sequential algorithms - a Huffman code is a bitstream, and where one symbol
// starts depends on where the one before it ended - so they cannot be spread
// over work-items. Instead one work-item runs the whole program, exactly as the
// CPU's loop does, and the CPU is left free to serve. It is slow; it runs on a
// cartridge import or a disaster-recovery archive and at no other time.
//
// The same file is compiled two ways: as an OpenCL program by the driver, and
// with -DHOST_TEST by a C compiler, so the algorithms can be checked against the
// Rust ones on a machine with no card.

#ifdef HOST_TEST
#include <stdio.h>
#include <stdlib.h>
typedef unsigned char uchar;
typedef unsigned int uint;
typedef unsigned short ushort;
#define KERNEL
#define GLOBAL
#define CONST static const
#define ID(x) 0
#else
#define KERNEL __kernel
#define GLOBAL __global
// OpenCL wants program-scope tables in the constant address space.
#define CONST __constant
#define ID(x) get_global_id(x)
#endif

// ---------------------------------------------------------------- checksums

// Bitwise on purpose: a table would have to live at program scope, and OpenCL
// only allows `__constant` there. One work-item has no cache to lose.
static uint crc32_of(GLOBAL const uchar *data, uint n) {
    uint crc = 0xffffffffu;
    for (uint i = 0; i < n; i++) {
        crc ^= data[i];
        for (int k = 0; k < 8; k++) {
            crc = (crc & 1u) ? (0xedb88320u ^ (crc >> 1)) : (crc >> 1);
        }
    }
    return crc ^ 0xffffffffu;
}

static uint adler32_of(GLOBAL const uchar *data, uint n) {
    uint a = 1, b = 0;
    for (uint i = 0; i < n; i++) {
        a = (a + data[i]) % 65521u;
        b = (b + a) % 65521u;
    }
    return (b << 16) | a;
}

// ----------------------------------------------------------------- bit read

typedef struct {
    GLOBAL const uchar *data;
    uint n;
    uint byte;
    uint bit;
    int ok;
} BitReader;

static uint br_bits(BitReader *br, uint n) {
    uint v = 0;
    for (uint i = 0; i < n; i++) {
        if (br->byte >= br->n) {
            br->ok = 0;
            return 0;
        }
        uint b = (br->data[br->byte] >> br->bit) & 1u;
        v |= b << i;
        br->bit++;
        if (br->bit == 8) {
            br->bit = 0;
            br->byte++;
        }
    }
    return v;
}

static void br_align(BitReader *br) {
    if (br->bit != 0) {
        br->bit = 0;
        br->byte++;
    }
}

// ------------------------------------------------------------------ huffman

typedef struct {
    ushort counts[16];
    ushort symbols[288];
} Huffman;

static void huff_build(Huffman *h, const uchar *lengths, uint n) {
    for (int i = 0; i < 16; i++) h->counts[i] = 0;
    for (uint i = 0; i < n; i++) h->counts[lengths[i]]++;
    h->counts[0] = 0;
    ushort offs[16];
    offs[0] = 0;
    for (int i = 1; i < 16; i++) offs[i] = (ushort)(offs[i - 1] + h->counts[i - 1]);
    for (uint s = 0; s < n; s++) {
        uchar l = lengths[s];
        if (l != 0) h->symbols[offs[l]++] = (ushort)s;
    }
}

// Returns 0 on a bad code or a truncated stream.
static int huff_decode(const Huffman *h, BitReader *br, uint *sym) {
    int code = 0, first = 0, index = 0;
    for (uint len = 1; len <= 15; len++) {
        code |= (int)br_bits(br, 1);
        if (!br->ok) return 0;
        int count = (int)h->counts[len];
        if (code - count < first) {
            *sym = h->symbols[index + (code - first)];
            return 1;
        }
        index += count;
        first += count;
        first <<= 1;
        code <<= 1;
    }
    return 0;
}

CONST uint LEN_BASE[29] = {
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115,
    131, 163, 195, 227, 258,
};
CONST uint LEN_EXTRA[29] = {
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
};
CONST uint DIST_BASE[30] = {
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
};
CONST uint DIST_EXTRA[30] = {
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12,
    13, 13,
};
CONST uint CL_ORDER[19] = {
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
};

// ---------------------------------------------------------------- inflate

static int inflate_block(BitReader *br, GLOBAL uchar *out, uint cap, uint *olen,
                         const Huffman *lit, const Huffman *dist) {
    for (;;) {
        uint sym;
        if (!huff_decode(lit, br, &sym)) return 0;
        if (sym < 256) {
            if (*olen >= cap) return 0;
            out[(*olen)++] = (uchar)sym;
        } else if (sym == 256) {
            return 1;
        } else {
            uint idx = sym - 257;
            if (idx >= 29) return 0;
            uint len = LEN_BASE[idx] + br_bits(br, LEN_EXTRA[idx]);
            if (!br->ok) return 0;
            if (*olen + len > cap) return 0;
            uint dsym;
            if (!huff_decode(dist, br, &dsym)) return 0;
            if (dsym >= 30) return 0;
            uint d = DIST_BASE[dsym] + br_bits(br, DIST_EXTRA[dsym]);
            if (!br->ok) return 0;
            if (d == 0 || d > *olen) return 0;
            uint start = *olen - d;
            for (uint k = 0; k < len; k++) out[*olen + k] = out[start + k];
            *olen += len;
        }
    }
}

// The whole deflate stream: stored, fixed and dynamic blocks.
static int inflate_into(BitReader *br, GLOBAL uchar *out, uint cap, uint *olen) {
    for (;;) {
        uint last = br_bits(br, 1);
        uint kind = br_bits(br, 2);
        if (!br->ok) return 0;
        if (kind == 0) {
            br_align(br);
            uint len = br_bits(br, 16);
            uint nlen = br_bits(br, 16);
            if (!br->ok) return 0;
            if (len != ((~nlen) & 0xffffu)) return 0;
            if (*olen + len > cap) return 0;
            for (uint i = 0; i < len; i++) {
                uint b = br_bits(br, 8);
                if (!br->ok) return 0;
                out[(*olen)++] = (uchar)b;
            }
        } else if (kind == 1) {
            uchar lit_lengths[288];
            uchar dist_lengths[32];
            for (uint i = 0; i < 288; i++) {
                lit_lengths[i] = (uchar)(i < 144 ? 8 : i < 256 ? 9 : i < 280 ? 7 : 8);
            }
            for (uint i = 0; i < 32; i++) dist_lengths[i] = 5;
            Huffman lit, dist;
            huff_build(&lit, lit_lengths, 288);
            huff_build(&dist, dist_lengths, 32);
            if (!inflate_block(br, out, cap, olen, &lit, &dist)) return 0;
        } else if (kind == 2) {
            uint hlit = br_bits(br, 5) + 257;
            uint hdist = br_bits(br, 5) + 1;
            uint hclen = br_bits(br, 4) + 4;
            if (!br->ok) return 0;
            if (hlit > 288 || hdist > 32) return 0;
            uchar cl_lengths[19];
            for (int i = 0; i < 19; i++) cl_lengths[i] = 0;
            for (uint i = 0; i < hclen; i++) {
                uint v = br_bits(br, 3);
                if (!br->ok) return 0;
                cl_lengths[CL_ORDER[i]] = (uchar)v;
            }
            Huffman cl;
            huff_build(&cl, cl_lengths, 19);
            uchar lengths[320];
            uint total = hlit + hdist;
            for (uint i = 0; i < total; i++) lengths[i] = 0;
            uint i = 0;
            while (i < total) {
                uint sym;
                if (!huff_decode(&cl, br, &sym)) return 0;
                if (sym <= 15) {
                    lengths[i++] = (uchar)sym;
                } else if (sym == 16) {
                    if (i == 0) return 0;
                    uchar prev = lengths[i - 1];
                    uint repeat = 3 + br_bits(br, 2);
                    if (!br->ok) return 0;
                    for (uint k = 0; k < repeat && i < total; k++) lengths[i++] = prev;
                } else if (sym == 17) {
                    uint repeat = 3 + br_bits(br, 3);
                    if (!br->ok) return 0;
                    i += repeat;
                    if (i > total) i = total;
                } else if (sym == 18) {
                    uint repeat = 11 + br_bits(br, 7);
                    if (!br->ok) return 0;
                    i += repeat;
                    if (i > total) i = total;
                } else {
                    return 0;
                }
            }
            Huffman lit, dist;
            huff_build(&lit, lengths, hlit);
            huff_build(&dist, lengths + hlit, hdist);
            if (!inflate_block(br, out, cap, olen, &lit, &dist)) return 0;
        } else {
            return 0;
        }
        if (last == 1) return 1;
    }
}

// ------------------------------------------------------------------- decode

// Decode one gzip or zlib member. `res[0]` is 0 on success, else an error code;
// `res[1]` is the output length. A gzip member is checked against its own CRC
// and length trailer here, on the card, so a decoder mistake cannot be mistaken
// for a successful decode.
KERNEL void gunzip(GLOBAL const uchar *in, uint n, GLOBAL uchar *out, uint cap,
                   uint zlib, GLOBAL uint *res) {
    if (ID(0) != 0) return;
    res[0] = 1;
    res[1] = 0;
    uint at = 0;
    uint olen = 0;
    int first = 1;
    // A gzip file may be several members in a row (RFC 1952 allows it, and the
    // card writes one member per slice), so keep going while another follows.
    for (;;) {
        if (zlib) {
            if (n < 6) return;
            uchar cmf = in[0], flg = in[1];
            if ((cmf & 0x0f) != 8) { res[0] = 2; return; }
            if ((((uint)cmf << 8) | flg) % 31 != 0) { res[0] = 2; return; }
            if (flg & 0x20) { res[0] = 3; return; }
            at = 2;
        } else {
            if (at + 18 > n || in[at] != 0x1f || in[at + 1] != 0x8b) {
                if (first) { res[0] = 2; return; }
                break;
            }
            uint flags = in[at + 3];
            uint p = at + 10;
            if (flags & 0x04) {
                if (p + 2 > n) { res[0] = 2; return; }
                uint xlen = (uint)in[p] | ((uint)in[p + 1] << 8);
                p += 2 + xlen;
            }
            if (flags & 0x08) {
                while (p < n && in[p] != 0) p++;
                p++;
            }
            if (flags & 0x10) {
                while (p < n && in[p] != 0) p++;
                p++;
            }
            if (flags & 0x02) p += 2;
            if (p >= n) { res[0] = 2; return; }
            at = p;
        }
        BitReader br;
        br.data = in + at;
        br.n = n - at;
        br.byte = 0;
        br.bit = 0;
        br.ok = 1;
        uint member = 0;
        if (!inflate_into(&br, out + olen, cap - olen, &member)) { res[0] = 4; return; }
        if (!br.ok) { res[0] = 4; return; }
        uint end = at + br.byte + (br.bit != 0 ? 1u : 0u);
        if (zlib) {
            if (end + 4 > n) { res[0] = 5; return; }
            uint want = ((uint)in[end] << 24) | ((uint)in[end + 1] << 16) |
                        ((uint)in[end + 2] << 8) | (uint)in[end + 3];
            if (adler32_of(out + olen, member) != want) { res[0] = 6; return; }
            olen += member;
            break;
        }
        if (end + 8 > n) { res[0] = 5; return; }
        uint want = (uint)in[end] | ((uint)in[end + 1] << 8) | ((uint)in[end + 2] << 16) |
                    ((uint)in[end + 3] << 24);
        uint size = (uint)in[end + 4] | ((uint)in[end + 5] << 8) | ((uint)in[end + 6] << 16) |
                    ((uint)in[end + 7] << 24);
        if (size != member) { res[0] = 6; return; }
        if (crc32_of(out + olen, member) != want) { res[0] = 6; return; }
        olen += member;
        at = end + 8;
        first = 0;
    }
    res[0] = 0;
    res[1] = olen;
}

// ------------------------------------------------------------------- encode

typedef struct {
    GLOBAL uchar *out;
    uint cap;
    uint len;
    uint acc;
    uint bits;
    int ok;
} BitWriter;

static void bw_put(BitWriter *bw, uint value, uint n) {
    for (uint i = 0; i < n; i++) {
        bw->acc |= ((value >> i) & 1u) << bw->bits;
        bw->bits++;
        if (bw->bits == 8) {
            if (bw->len < bw->cap) bw->out[bw->len++] = (uchar)bw->acc;
            else bw->ok = 0;
            bw->acc = 0;
            bw->bits = 0;
        }
    }
}

// Deflate writes codes most-significant bit first inside the code.
static void bw_code(BitWriter *bw, uint code, uint n) {
    for (uint i = 0; i < n; i++) bw_put(bw, (code >> (n - 1 - i)) & 1u, 1);
}

static void bw_flush(BitWriter *bw) {
    if (bw->bits != 0) {
        if (bw->len < bw->cap) bw->out[bw->len++] = (uchar)bw->acc;
        else bw->ok = 0;
        bw->acc = 0;
        bw->bits = 0;
    }
}

static void find_length(uint len, uint *idx, uint *extra) {
    uint i = 28;
    while (i > 0 && LEN_BASE[i] > len) i--;
    *idx = i;
    *extra = len - LEN_BASE[i];
}

static void find_distance(uint dist, uint *idx, uint *extra) {
    uint i = 29;
    while (i > 0 && DIST_BASE[i] > dist) i--;
    *idx = i;
    *extra = dist - DIST_BASE[i];
}

static void fixed_lit(BitWriter *bw, uint b) {
    if (b < 144) bw_code(bw, 0x30 + b, 8);
    else if (b < 256) bw_code(bw, 0x190 + (b - 144), 9);
    else if (b < 280) bw_code(bw, b - 256, 7);
    else bw_code(bw, 0xc0 + (b - 280), 8);
}

// A block with fixed tables: no header, so best for tiny blocks.
static void emit_fixed(BitWriter *bw, GLOBAL const uint *tok, uint from, uint to, int last) {
    bw_put(bw, last ? 1u : 0u, 1);
    bw_put(bw, 1, 2);
    for (uint i = from; i < to; i++) {
        uint t = tok[i];
        uint len = t >> 16;
        if (len == 0) {
            fixed_lit(bw, t & 0xffu);
        } else {
            uint idx, extra;
            find_length(len, &idx, &extra);
            fixed_lit(bw, 257 + idx);
            if (LEN_EXTRA[idx]) bw_put(bw, extra, LEN_EXTRA[idx]);
            find_distance(t & 0xffffu, &idx, &extra);
            bw_code(bw, idx, 5);
            if (DIST_EXTRA[idx]) bw_put(bw, extra, DIST_EXTRA[idx]);
        }
    }
    fixed_lit(bw, 256);
}

// Code lengths for `freq`: merge the two lightest until one tree is left, then
// read each leaf's depth. Returns 0 when a depth exceeds `limit` or the code is
// not exactly full - the caller then sends that block with the fixed tables,
// because a tree that does not fit would produce a stream no decoder accepts.
static int huff_lengths(const uint *freq, uint n, uint limit, uchar *lengths) {
    uint w[600];
    int par[600];
    uint sym[600];
    uint live[600];
    uint cnt = 0;
    for (uint i = 0; i < n; i++) lengths[i] = 0;
    for (uint i = 0; i < n; i++) {
        if (freq[i] > 0) {
            w[cnt] = freq[i];
            par[cnt] = -1;
            sym[cnt] = i;
            live[cnt] = cnt;
            cnt++;
        }
    }
    if (cnt == 0) return 0;
    if (cnt == 1) {
        uint only = sym[0];
        lengths[only] = 1;
        lengths[only == 0 ? 1 : 0] = 1;
        return 1;
    }
    uint next = cnt;
    uint live_n = cnt;
    while (live_n > 1) {
        uint ai = 0, bi = 1;
        if (w[live[ai]] > w[live[bi]]) { ai = 1; bi = 0; }
        for (uint i = 2; i < live_n; i++) {
            if (w[live[i]] < w[live[ai]]) { bi = ai; ai = i; }
            else if (w[live[i]] < w[live[bi]]) { bi = i; }
        }
        uint a = live[ai], b = live[bi];
        if (next >= 600) return 0;
        w[next] = w[a] + w[b];
        par[next] = -1;
        par[a] = (int)next;
        par[b] = (int)next;
        uint k = 0;
        for (uint i = 0; i < live_n; i++) {
            if (live[i] != a && live[i] != b) live[k++] = live[i];
        }
        live[k++] = next++;
        live_n = k;
    }
    uint kraft = 0;
    for (uint i = 0; i < cnt; i++) {
        uint d = 0;
        int p = par[i];
        while (p != -1) {
            d++;
            p = par[p];
        }
        if (d == 0 || d > limit) return 0;
        lengths[sym[i]] = (uchar)d;
        kraft += 1u << (limit - d);
    }
    if (kraft != (1u << limit)) return 0;
    return 1;
}

static void canonical(const uchar *lengths, uint n, ushort *codes) {
    ushort bl_count[16];
    ushort next_code[16];
    for (int i = 0; i < 16; i++) { bl_count[i] = 0; next_code[i] = 0; }
    for (uint i = 0; i < n; i++) {
        if (lengths[i]) bl_count[lengths[i]]++;
    }
    ushort code = 0;
    for (uint bits = 1; bits < 16; bits++) {
        code = (ushort)((code + bl_count[bits - 1]) << 1);
        next_code[bits] = code;
    }
    for (uint i = 0; i < n; i++) {
        codes[i] = 0;
        if (lengths[i]) codes[i] = next_code[lengths[i]]++;
    }
}

typedef struct {
    uchar sym;
    uchar extra_bits;
    ushort extra;
} Rle;

static uint rle_lengths(const uchar *seq, uint n, Rle *out, uint cap, uint *freq) {
    for (int i = 0; i < 19; i++) freq[i] = 0;
    uint at = 0, o = 0;
    while (at < n) {
        uchar v = seq[at];
        uint run = 1;
        while (at + run < n && seq[at + run] == v) run++;
        at += run;
        if (v == 0) {
            uint left = run;
            while (left >= 11) {
                uint take = left < 138 ? left : 138;
                if (o < cap) { out[o].sym = 18; out[o].extra_bits = 7; out[o].extra = (ushort)(take - 11); }
                o++;
                freq[18]++;
                left -= take;
            }
            if (left >= 3) {
                uint take = left < 10 ? left : 10;
                if (o < cap) { out[o].sym = 17; out[o].extra_bits = 3; out[o].extra = (ushort)(take - 3); }
                o++;
                freq[17]++;
                left -= take;
            }
            while (left > 0) {
                if (o < cap) { out[o].sym = 0; out[o].extra_bits = 0; out[o].extra = 0; }
                o++;
                freq[0]++;
                left--;
            }
        } else {
            if (o < cap) { out[o].sym = v; out[o].extra_bits = 0; out[o].extra = 0; }
            o++;
            freq[v]++;
            uint left = run - 1;
            while (left >= 3) {
                uint take = left < 6 ? left : 6;
                if (o < cap) { out[o].sym = 16; out[o].extra_bits = 2; out[o].extra = (ushort)(take - 3); }
                o++;
                freq[16]++;
                left -= take;
            }
            while (left > 0) {
                if (o < cap) { out[o].sym = v; out[o].extra_bits = 0; out[o].extra = 0; }
                o++;
                freq[v]++;
                left--;
            }
        }
    }
    return o;
}

static void emit_dynamic(BitWriter *bw, GLOBAL const uint *tok, uint from, uint to, int last) {
    uint lit_freq[286];
    uint dist_freq[30];
    for (int i = 0; i < 286; i++) lit_freq[i] = 0;
    for (int i = 0; i < 30; i++) dist_freq[i] = 0;
    for (uint i = from; i < to; i++) {
        uint t = tok[i];
        uint len = t >> 16;
        if (len == 0) {
            lit_freq[t & 0xffu]++;
        } else {
            uint idx, extra;
            find_length(len, &idx, &extra);
            lit_freq[257 + idx]++;
            find_distance(t & 0xffffu, &idx, &extra);
            dist_freq[idx]++;
        }
    }
    lit_freq[256]++;
    uchar lit_len[286];
    uchar dist_len[30];
    if (!huff_lengths(lit_freq, 286, 15, lit_len)) {
        emit_fixed(bw, tok, from, to, last);
        return;
    }
    uint dist_any = 0;
    for (int i = 0; i < 30; i++) if (dist_freq[i]) dist_any = 1;
    if (!dist_any) {
        for (int i = 0; i < 30; i++) dist_len[i] = 0;
        dist_len[0] = 1;
    } else if (!huff_lengths(dist_freq, 30, 15, dist_len)) {
        emit_fixed(bw, tok, from, to, last);
        return;
    }
    uint hlit = 286, hdist = 30;
    while (hlit > 257 && lit_len[hlit - 1] == 0) hlit--;
    while (hdist > 1 && dist_len[hdist - 1] == 0) hdist--;
    uchar seq[320];
    for (uint i = 0; i < hlit; i++) seq[i] = lit_len[i];
    for (uint i = 0; i < hdist; i++) seq[hlit + i] = dist_len[i];
    Rle rle[340];
    uint cl_freq[19];
    uint ncl = rle_lengths(seq, hlit + hdist, rle, 340, cl_freq);
    uchar cl_len[19];
    if (!huff_lengths(cl_freq, 19, 7, cl_len)) {
        emit_fixed(bw, tok, from, to, last);
        return;
    }
    uint hclen = 19;
    while (hclen > 4 && cl_len[CL_ORDER[hclen - 1]] == 0) hclen--;

    bw_put(bw, last ? 1u : 0u, 1);
    bw_put(bw, 2, 2);
    bw_put(bw, hlit - 257, 5);
    bw_put(bw, hdist - 1, 5);
    bw_put(bw, hclen - 4, 4);
    for (uint i = 0; i < hclen; i++) bw_put(bw, cl_len[CL_ORDER[i]], 3);
    ushort cl_code[19];
    canonical(cl_len, 19, cl_code);
    for (uint i = 0; i < ncl; i++) {
        bw_code(bw, cl_code[rle[i].sym], cl_len[rle[i].sym]);
        if (rle[i].extra_bits) bw_put(bw, rle[i].extra, rle[i].extra_bits);
    }
    ushort lit_code[286];
    ushort dist_code[30];
    canonical(lit_len, 286, lit_code);
    canonical(dist_len, 30, dist_code);
    for (uint i = from; i < to; i++) {
        uint t = tok[i];
        uint len = t >> 16;
        if (len == 0) {
            uint b = t & 0xffu;
            bw_code(bw, lit_code[b], lit_len[b]);
        } else {
            uint idx, extra;
            find_length(len, &idx, &extra);
            bw_code(bw, lit_code[257 + idx], lit_len[257 + idx]);
            if (LEN_EXTRA[idx]) bw_put(bw, extra, LEN_EXTRA[idx]);
            find_distance(t & 0xffffu, &idx, &extra);
            bw_code(bw, dist_code[idx], dist_len[idx]);
            if (DIST_EXTRA[idx]) bw_put(bw, extra, DIST_EXTRA[idx]);
        }
    }
    bw_code(bw, lit_code[256], lit_len[256]);
}

// ---------------------------------------------------------------- LZ77

static uint hash3(GLOBAL const uchar *in, uint pos) {
    uint v = (uint)in[pos] | ((uint)in[pos + 1] << 8) | ((uint)in[pos + 2] << 16);
    return (v * 0x1e35a7bdu) >> 16;
}

static void lz_insert(GLOBAL const uchar *in, uint n, GLOBAL uint *head, GLOBAL uint *prev, uint pos) {
    if (pos + 3 > n) return;
    uint h = hash3(in, pos);
    prev[pos & 32767u] = head[h];
    head[h] = pos;
}

static void lz_search(GLOBAL const uchar *in, uint n, GLOBAL const uint *head,
                      GLOBAL const uint *prev, uint pos, uint *best_len, uint *best_dist) {
    *best_len = 0;
    *best_dist = 0;
    if (pos + 3 > n) return;
    uint h = hash3(in, pos);
    uint limit = n - pos;
    if (limit > 258) limit = 258;
    uint cand = head[h];
    uint tries = 0;
    while (cand != 0xffffffffu && tries < 128) {
        if (cand >= pos) break;
        uint dist = pos - cand;
        if (dist > 32768) {
            cand = prev[cand & 32767u];
            tries++;
            continue;
        }
        uint len = 0;
        while (len < limit && in[cand + len] == in[pos + len]) len++;
        if (len > *best_len) {
            *best_len = len;
            *best_dist = dist;
            if (len >= limit) break;
        }
        cand = prev[cand & 32767u];
        tries++;
    }
}

// LZ77 for one slice: hash chains, newest first, one step of lazy matching.
// `head` and `prev` are scratch the caller owns (65536 + 32768 uints).
static void lz77(GLOBAL const uchar *in, uint n, GLOBAL uint *tok, uint *ntok,
                 GLOBAL uint *head, GLOBAL uint *prev) {
    for (uint i = 0; i < 65536; i++) head[i] = 0xffffffffu;
    for (uint i = 0; i < 32768; i++) prev[i] = 0xffffffffu;
    uint out = 0;
    uint pos = 0;
    while (pos < n) {
        uint len, dist;
        lz_search(in, n, head, prev, pos, &len, &dist);
        if (len >= 3) {
            if (pos + 1 < n) {
                lz_insert(in, n, head, prev, pos);
                uint nl, nd;
                lz_search(in, n, head, prev, pos + 1, &nl, &nd);
                if (nl > len) {
                    tok[out++] = (uint)in[pos];
                    pos++;
                    continue;
                }
            } else {
                lz_insert(in, n, head, prev, pos);
            }
            tok[out++] = (len << 16) | (dist & 0xffffu);
            for (uint k = 1; k < len; k++) lz_insert(in, n, head, prev, pos + k);
            pos += len;
        } else {
            tok[out++] = (uint)in[pos];
            lz_insert(in, n, head, prev, pos);
            pos++;
        }
    }
    *ntok = out;
}

// One gzip member for one slice: header, deflate, CRC and length trailer.
KERNEL void gzip(GLOBAL const uchar *in, uint n, GLOBAL uchar *out, uint cap,
                 GLOBAL uint *tok, GLOBAL uint *head, GLOBAL uint *prev, GLOBAL uint *res) {
    if (ID(0) != 0) return;
    res[0] = 1;
    res[1] = 0;
    if (cap < n + n / 8 + 2048) return;
    uint ntok = 0;
    lz77(in, n, tok, &ntok, head, prev);
    out[0] = 0x1f; out[1] = 0x8b; out[2] = 8; out[3] = 0;
    out[4] = out[5] = out[6] = out[7] = 0;
    out[8] = 0; out[9] = 3;
    BitWriter bw;
    bw.out = out;
    bw.cap = cap;
    bw.len = 10;
    bw.acc = 0;
    bw.bits = 0;
    bw.ok = 1;
    uint at = 0;
    while (at < ntok) {
        uint end = at + 131072;
        if (end > ntok) end = ntok;
        uint count = end - at;
        if (count < 64) {
            emit_fixed(&bw, tok, at, end, end == ntok);
        } else {
            emit_dynamic(&bw, tok, at, end, end == ntok);
        }
        at = end;
    }
    if (ntok == 0) emit_fixed(&bw, tok, 0, 0, 1);
    bw_flush(&bw);
    if (!bw.ok || bw.len + 8 > cap) { res[0] = 7; return; }
    uint crc = crc32_of(in, n);
    uint len = bw.len;
    out[len++] = (uchar)(crc);
    out[len++] = (uchar)(crc >> 8);
    out[len++] = (uchar)(crc >> 16);
    out[len++] = (uchar)(crc >> 24);
    out[len++] = (uchar)(n);
    out[len++] = (uchar)(n >> 8);
    out[len++] = (uchar)(n >> 16);
    out[len++] = (uchar)(n >> 24);
    res[0] = 0;
    res[1] = len;
}

// ------------------------------------------------------------------ host test

#ifdef HOST_TEST
static uchar *slurp(const char *path, uint *n) {
    FILE *f = fopen(path, "rb");
    if (!f) return 0;
    fseek(f, 0, SEEK_END);
    long sz = ftell(f);
    fseek(f, 0, SEEK_SET);
    uchar *buf = (uchar *)malloc((size_t)sz + 1);
    if (!buf) return 0;
    if (fread(buf, 1, (size_t)sz, f) != (size_t)sz) { fclose(f); return 0; }
    fclose(f);
    *n = (uint)sz;
    return buf;
}

int main(int argc, char **argv) {
    if (argc < 4) {
        fprintf(stderr, "usage: %s c|d|z in out\n", argv[0]);
        return 2;
    }
    uint n = 0;
    uchar *in = slurp(argv[2], &n);
    if (!in) { fprintf(stderr, "cannot read %s\n", argv[2]); return 2; }
    uint cap = argv[1][0] == 'c' ? n + n / 8 + 4096 : n * 64 + 65536;
    uchar *out = (uchar *)malloc(cap);
    uint *res = (uint *)malloc(8);
    if (!out || !res) return 2;
    if (argv[1][0] == 'c') {
        uint *tok = (uint *)malloc((size_t)(n + 16) * 4);
        uint *head = (uint *)malloc(65536 * 4);
        uint *prev = (uint *)malloc(32768 * 4);
        if (!tok || !head || !prev) return 2;
        gzip(in, n, out, cap, tok, head, prev, res);
    } else {
        gunzip(in, n, out, cap, argv[1][0] == 'z' ? 1u : 0u, res);
    }
    if (res[0] != 0) {
        fprintf(stderr, "error %u\n", res[0]);
        return 1;
    }
    FILE *f = fopen(argv[3], "wb");
    if (!f) return 2;
    fwrite(out, 1, res[1], f);
    fclose(f);
    printf("%u -> %u\n", n, res[1]);
    return 0;
}
#endif
