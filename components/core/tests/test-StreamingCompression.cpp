#include <cstring>
#include <string>

#include <boost/filesystem.hpp>
#include <Catch2/single_include/catch2/catch.hpp>
#include <zstd.h>

#include "../src/clp/ReadOnlyMemoryMappedFile.hpp"
#include "../src/clp/streaming_compression/passthrough/Compressor.hpp"
#include "../src/clp/streaming_compression/passthrough/Decompressor.hpp"
#include "../src/clp/streaming_compression/zstd/Compressor.hpp"
#include "../src/clp/streaming_compression/zstd/Decompressor.hpp"

using clp::ErrorCode_Success;
using clp::FileWriter;

TEST_CASE("StreamingCompression", "[StreamingCompression]") {
    // Initialize data to test compression and decompression
    size_t uncompressed_data_size = 128L * 1024 * 1024;  // 128MB
    char* uncompressed_data = new char[uncompressed_data_size];
    for (size_t i = 0; i < uncompressed_data_size; ++i) {
        uncompressed_data[i] = (char)('a' + (i % 26));
    }

    // Create output buffer
    char* decompressed_data = new char[uncompressed_data_size];

    SECTION("zstd single phase compression") {
        // Clear output buffer
        memset(decompressed_data, 0, uncompressed_data_size);
        std::string compressed_file_path = "compressed_file.zstd.bin.1";

        // Compress
        FileWriter file_writer;
        file_writer.open(compressed_file_path, FileWriter::OpenMode::CREATE_FOR_WRITING);
        clp::streaming_compression::zstd::Compressor compressor;
        compressor.open(file_writer);
        compressor.write(uncompressed_data, ZSTD_CStreamInSize());
        compressor.write(uncompressed_data, uncompressed_data_size / 100);
        compressor.write(uncompressed_data, uncompressed_data_size / 50);
        compressor.write(uncompressed_data, uncompressed_data_size / 25);
        compressor.write(uncompressed_data, uncompressed_data_size / 10);
        compressor.write(uncompressed_data, uncompressed_data_size / 5);
        compressor.write(uncompressed_data, uncompressed_data_size / 2);
        compressor.write(uncompressed_data, uncompressed_data_size);
        compressor.close();
        file_writer.close();

        // Decompress
        clp::streaming_compression::zstd::Decompressor decompressor;
        REQUIRE(ErrorCode_Success == decompressor.open(compressed_file_path));
        size_t uncompressed_bytes = 0;
        REQUIRE(ErrorCode_Success
                == decompressor.get_decompressed_stream_region(
                        uncompressed_bytes,
                        decompressed_data,
                        ZSTD_CStreamInSize()
                ));
        REQUIRE(memcmp(uncompressed_data, decompressed_data, ZSTD_CStreamInSize()) == 0);
        memset(decompressed_data, 0, uncompressed_data_size);
        uncompressed_bytes += ZSTD_CStreamInSize();

        REQUIRE(ErrorCode_Success
                == decompressor.get_decompressed_stream_region(
                        uncompressed_bytes,
                        decompressed_data,
                        uncompressed_data_size / 100
                ));
        REQUIRE(memcmp(uncompressed_data, decompressed_data, uncompressed_data_size / 100) == 0);
        memset(decompressed_data, 0, uncompressed_data_size);
        uncompressed_bytes += uncompressed_data_size / 100;
        REQUIRE(ErrorCode_Success
                == decompressor.get_decompressed_stream_region(
                        uncompressed_bytes,
                        decompressed_data,
                        uncompressed_data_size / 50
                ));
        REQUIRE(memcmp(uncompressed_data, decompressed_data, uncompressed_data_size / 50) == 0);
        memset(decompressed_data, 0, uncompressed_data_size);
        uncompressed_bytes += uncompressed_data_size / 50;
        REQUIRE(ErrorCode_Success
                == decompressor.get_decompressed_stream_region(
                        uncompressed_bytes,
                        decompressed_data,
                        uncompressed_data_size / 25
                ));
        REQUIRE(memcmp(uncompressed_data, decompressed_data, uncompressed_data_size / 25) == 0);
        memset(decompressed_data, 0, uncompressed_data_size);
        uncompressed_bytes += uncompressed_data_size / 25;
        REQUIRE(ErrorCode_Success
                == decompressor.get_decompressed_stream_region(
                        uncompressed_bytes,
                        decompressed_data,
                        uncompressed_data_size / 10
                ));
        REQUIRE(memcmp(uncompressed_data, decompressed_data, uncompressed_data_size / 10) == 0);
        memset(decompressed_data, 0, uncompressed_data_size);
        uncompressed_bytes += uncompressed_data_size / 10;
        REQUIRE(ErrorCode_Success
                == decompressor.get_decompressed_stream_region(
                        uncompressed_bytes,
                        decompressed_data,
                        uncompressed_data_size / 5
                ));
        REQUIRE(memcmp(uncompressed_data, decompressed_data, uncompressed_data_size / 5) == 0);
        memset(decompressed_data, 0, uncompressed_data_size);
        uncompressed_bytes += uncompressed_data_size / 5;
        REQUIRE(ErrorCode_Success
                == decompressor.get_decompressed_stream_region(
                        uncompressed_bytes,
                        decompressed_data,
                        uncompressed_data_size / 2
                ));
        REQUIRE(memcmp(uncompressed_data, decompressed_data, uncompressed_data_size / 2) == 0);
        memset(decompressed_data, 0, uncompressed_data_size);
        uncompressed_bytes += uncompressed_data_size / 2;
        REQUIRE(ErrorCode_Success
                == decompressor.get_decompressed_stream_region(
                        uncompressed_bytes,
                        decompressed_data,
                        uncompressed_data_size
                ));
        REQUIRE(memcmp(uncompressed_data, decompressed_data, uncompressed_data_size) == 0);
        memset(decompressed_data, 0, uncompressed_data_size);
        uncompressed_bytes += uncompressed_data_size;

        // Cleanup
        boost::filesystem::remove(compressed_file_path);
    }

    SECTION("passthrough compression") {
        // Clear output buffer
        memset(decompressed_data, 0, uncompressed_data_size);
        std::string compressed_file_path = "compressed_file.passthrough.bin";

        // Compress
        FileWriter file_writer;
        file_writer.open(compressed_file_path, FileWriter::OpenMode::CREATE_FOR_WRITING);
        clp::streaming_compression::passthrough::Compressor compressor;
        compressor.open(file_writer);
        compressor.write(uncompressed_data, uncompressed_data_size / 100);
        compressor.write(uncompressed_data, uncompressed_data_size / 50);
        compressor.write(uncompressed_data, uncompressed_data_size / 25);
        compressor.write(uncompressed_data, uncompressed_data_size / 10);
        compressor.write(uncompressed_data, uncompressed_data_size / 5);
        compressor.write(uncompressed_data, uncompressed_data_size / 2);
        compressor.write(uncompressed_data, uncompressed_data_size);
        compressor.close();
        file_writer.close();

        // Decompress
        // Memory map compressed file
        clp::ReadOnlyMemoryMappedFile const memory_mapped_compressed_file{compressed_file_path};
        clp::streaming_compression::passthrough::Decompressor decompressor;
        auto const compressed_file_view{memory_mapped_compressed_file.get_view()};
        decompressor.open(compressed_file_view.data(), compressed_file_view.size());

        size_t uncompressed_bytes = 0;
        REQUIRE(ErrorCode_Success
                == decompressor.get_decompressed_stream_region(
                        uncompressed_bytes,
                        decompressed_data,
                        uncompressed_data_size / 100
                ));
        REQUIRE(memcmp(uncompressed_data, decompressed_data, uncompressed_data_size / 100) == 0);
        memset(decompressed_data, 0, uncompressed_data_size);
        uncompressed_bytes += uncompressed_data_size / 100;
        REQUIRE(ErrorCode_Success
                == decompressor.get_decompressed_stream_region(
                        uncompressed_bytes,
                        decompressed_data,
                        uncompressed_data_size / 50
                ));
        REQUIRE(memcmp(uncompressed_data, decompressed_data, uncompressed_data_size / 50) == 0);
        memset(decompressed_data, 0, uncompressed_data_size);
        uncompressed_bytes += uncompressed_data_size / 50;
        REQUIRE(ErrorCode_Success
                == decompressor.get_decompressed_stream_region(
                        uncompressed_bytes,
                        decompressed_data,
                        uncompressed_data_size / 25
                ));
        REQUIRE(memcmp(uncompressed_data, decompressed_data, uncompressed_data_size / 25) == 0);
        memset(decompressed_data, 0, uncompressed_data_size);
        uncompressed_bytes += uncompressed_data_size / 25;
        REQUIRE(ErrorCode_Success
                == decompressor.get_decompressed_stream_region(
                        uncompressed_bytes,
                        decompressed_data,
                        uncompressed_data_size / 10
                ));
        REQUIRE(memcmp(uncompressed_data, decompressed_data, uncompressed_data_size / 10) == 0);
        memset(decompressed_data, 0, uncompressed_data_size);
        uncompressed_bytes += uncompressed_data_size / 10;
        REQUIRE(ErrorCode_Success
                == decompressor.get_decompressed_stream_region(
                        uncompressed_bytes,
                        decompressed_data,
                        uncompressed_data_size / 5
                ));
        REQUIRE(memcmp(uncompressed_data, decompressed_data, uncompressed_data_size / 5) == 0);
        memset(decompressed_data, 0, uncompressed_data_size);
        uncompressed_bytes += uncompressed_data_size / 5;
        REQUIRE(ErrorCode_Success
                == decompressor.get_decompressed_stream_region(
                        uncompressed_bytes,
                        decompressed_data,
                        uncompressed_data_size / 2
                ));
        REQUIRE(memcmp(uncompressed_data, decompressed_data, uncompressed_data_size / 2) == 0);
        memset(decompressed_data, 0, uncompressed_data_size);
        uncompressed_bytes += uncompressed_data_size / 2;
        REQUIRE(ErrorCode_Success
                == decompressor.get_decompressed_stream_region(
                        uncompressed_bytes,
                        decompressed_data,
                        uncompressed_data_size
                ));
        REQUIRE(memcmp(uncompressed_data, decompressed_data, uncompressed_data_size) == 0);
        memset(decompressed_data, 0, uncompressed_data_size);
        uncompressed_bytes += uncompressed_data_size;

        // Cleanup
        boost::filesystem::remove(compressed_file_path);
    }

    // Cleanup
    delete[] uncompressed_data;
    delete[] decompressed_data;
}
