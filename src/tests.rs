// Реестр тестов: (имя, категория, функция). Порядок = порядок вывода.
// Категория печатается заголовком секции.

use crate::dev::TR;
use crate::t_data as d;
use crate::t_proto as p;

pub type Test = (&'static str, &'static str, fn(&str) -> TR);

pub fn all() -> Vec<Test> {
    vec![
        // smoke
        ("handshake", "smoke", d::t_handshake),
        ("read-sector-0", "smoke", d::t_read0),
        // целостность
        ("roundtrip", "integrity", d::t_roundtrip),
        ("multiblock-distinct-patterns", "integrity", d::t_multiblock),
        ("overwrite", "integrity", d::t_overwrite),
        ("persistence-across-reconnect", "integrity", p::t_persistence_reconnect),
        // раскладки дескрипторов
        ("header-split-8+8", "descriptors", d::t_hdr_split2),
        ("header-split-4x4", "descriptors", d::t_hdr_split4),
        ("header+data-one-desc (write)", "descriptors", d::t_hdr_data_contiguous_write),
        ("scatter-write-8x512", "descriptors", d::t_scatter_write),
        ("scatter-read-8x512", "descriptors", d::t_scatter_read),
        ("uneven-segments-512/1024/512/2048", "descriptors", d::t_uneven_segments),
        ("many-segments-64x512", "descriptors", d::t_many_segments),
        ("oversized-status-desc", "descriptors", d::t_status_oversized),
        ("indirect-descriptors", "descriptors", d::t_indirect_rw),
        // типы запросов
        ("flush", "req-types", d::t_flush),
        ("flush-nonzero-sector", "req-types", d::t_flush_nonzero_sector),
        ("get-id", "req-types", d::t_get_id),
        ("write-zeroes", "req-types", d::t_write_zeroes),
        ("write-zeroes-unmap", "req-types", d::t_write_zeroes_unmap),
        ("discard-then-rewrite", "req-types", d::t_discard),
        ("unsupported-request-type", "req-types", d::t_unsupported_type),
        ("zero-length-read", "req-types", d::t_zero_length_read),
        ("non-multiple-length", "req-types", d::t_nonmultiple_length),
        // границы
        ("last-sector", "bounds", d::t_last_sector),
        ("cross-capacity-boundary", "bounds", d::t_cross_capacity),
        ("beyond-capacity", "bounds", d::t_beyond_capacity),
        ("large-request-128k", "bounds", d::t_large_request),
        // механика очереди
        ("multi-outstanding-16", "vq-mechanics", d::t_multi_outstanding),
        ("out-of-order-completion", "vq-mechanics", d::t_out_of_order),
        ("used.len-read=data+status", "vq-mechanics", d::t_used_len_read),
        ("used.len-write=1", "vq-mechanics", d::t_used_len_write),
        ("spurious-kick", "vq-mechanics", d::t_spurious_kick),
        ("no-interrupt-flag", "vq-mechanics", d::t_no_interrupt),
        ("double-kick", "vq-mechanics", d::t_double_kick),
        // config
        ("config-capacity-consistency", "config", p::t_config_capacity_consistency),
        ("config-partial-read", "config", p::t_config_partial_read),
        ("get-features-stable", "config", p::t_get_features_stable),
        // жизненный цикл / злой ввод (демон обязан выжить)
        ("reconnect-stress", "lifecycle", p::t_reconnect_stress),
        ("double-set-owner", "hostile", p::t_double_set_owner),
        ("vring-num-zero", "hostile", p::t_vring_num_zero),
        ("vring-num-not-pow2", "hostile", p::t_vring_num_not_pow2),
        ("vring-num-too-big", "hostile", p::t_vring_num_too_big),
        ("vring-addr-unaligned", "hostile", p::t_vring_addr_unaligned),
        ("mem-table-empty", "hostile", p::t_mem_table_empty),
        ("vring-before-mem-table", "hostile", p::t_vring_before_mem),
    ]
}
