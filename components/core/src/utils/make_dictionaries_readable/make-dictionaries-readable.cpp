// C++ standard libraries
#include <string>

// Boost libraries
#include <boost/filesystem.hpp>

// spdlog
#include <spdlog/sinks/stdout_sinks.h>
#include <spdlog/spdlog.h>

// Project headers
#include "../../FileWriter.hpp"
#include "../../LogTypeDictionaryReader.hpp"
#include "../../VariableDictionaryReader.hpp"
#include "../../streaming_archive/Constants.hpp"
#include "CommandLineArguments.hpp"

using std::string;

int main (int argc, const char* argv[]) {
    // Program-wide initialization
    try {
        auto stderr_logger = spdlog::stderr_logger_st("stderr");
        spdlog::set_default_logger(stderr_logger);
        spdlog::set_pattern("%Y-%m-%d %H:%M:%S,%e [%l] %v");
    } catch (std::exception& e) {
        // NOTE: We can't log an exception if the logger couldn't be constructed
        return -1;
    }

    utils::make_dictionaries_readable::CommandLineArguments command_line_args("make-dictionaries-readable");
    auto parsing_result = command_line_args.parse_arguments(argc, argv);
    switch (parsing_result) {
        case CommandLineArgumentsBase::ParsingResult::Failure:
            return -1;
        case CommandLineArgumentsBase::ParsingResult::InfoCommand:
            return 0;
        case CommandLineArgumentsBase::ParsingResult::Success:
            // Continue processing
            break;
    }

    FileWriter file_writer;

    // Open log-type dictionary
    auto logtype_dict_path = boost::filesystem::path(command_line_args.get_archive_path()) / streaming_archive::cLogTypeDictFilename;
    auto logtype_segment_index_path = boost::filesystem::path(command_line_args.get_archive_path()) / streaming_archive::cLogTypeSegmentIndexFilename;
    LogTypeDictionaryReader logtype_dict;
    logtype_dict.open(logtype_dict_path.string(), logtype_segment_index_path.string());
    logtype_dict.read_new_entries();

    // Write readable dictionary
    auto readable_logtype_dict_path = boost::filesystem::path(command_line_args.get_output_dir()) / streaming_archive::cLogTypeDictFilename;
    readable_logtype_dict_path += ".hr";
    file_writer.open(readable_logtype_dict_path.string(), FileWriter::OpenMode::CREATE_FOR_WRITING);
    string human_readable_value;
    for (const auto& entry : logtype_dict.get_entries()) {
        const auto& value = entry.get_value();
        human_readable_value.clear();

        size_t constant_begin_pos = 0;
        for (size_t var_ix = 0; var_ix < entry.get_num_vars(); ++var_ix) {
            LogTypeDictionaryEntry::VarDelim var_delim;
            uint8_t num_integer_digits, num_fractional_digits;
            size_t var_pos = entry.get_var_info(var_ix, var_delim, num_integer_digits, num_fractional_digits);

            // Add the constant that's between the last variable and this one, with newlines escaped
            human_readable_value.append(value, constant_begin_pos, var_pos - constant_begin_pos);

            if (LogTypeDictionaryEntry::VarDelim::NonDouble == var_delim) {
                human_readable_value += "\\v";

                // Move past the variable delimiter
                constant_begin_pos = var_pos + 1;
            } else { // LogTypeDictionaryEntry::VarDelim::Double == var_delim
                human_readable_value += "\\ff";

                // Move past the variable delimiter and the double's precision
                constant_begin_pos = var_pos + 2;
            }
        }
        // Append remainder of value, if any
        if (constant_begin_pos < value.length()) {
            human_readable_value.append(value, constant_begin_pos, string::npos);
        }

        file_writer.write_string(replace_characters("\n", "n", human_readable_value, true));
        file_writer.write_char('\n');
    }
    file_writer.close();

    logtype_dict.close();

    // Open variables dictionary
    auto var_dict_path = boost::filesystem::path(command_line_args.get_archive_path()) / streaming_archive::cVarDictFilename;
    auto var_segment_index_path = boost::filesystem::path(command_line_args.get_archive_path()) / streaming_archive::cVarSegmentIndexFilename;
    VariableDictionaryReader var_dict;
    var_dict.open(var_dict_path.string(), var_segment_index_path.string());
    var_dict.read_new_entries();

    // Write readable dictionary
    auto readable_var_dict_path = boost::filesystem::path(command_line_args.get_output_dir()) / streaming_archive::cVarDictFilename;
    readable_var_dict_path += ".hr";
    file_writer.open(readable_var_dict_path.string(), FileWriter::OpenMode::CREATE_FOR_WRITING);
    for (const auto& entry : var_dict.get_entries()) {
        file_writer.write_string(entry.get_value());
        file_writer.write_char('\n');
    }
    file_writer.close();

    var_dict.close();

    return 0;
}