#include <iostream>
#include <system_error>

#include <Catch2/single_include/catch2/catch.hpp>

#include "../src/clp/GenericErrorCode.hpp"
#include "../src/clp/RegexErrorCode.hpp"

TEST_CASE("test_error_code", "[ERRORCODE]") {
    std::error_code err_question{clp::RegexErrorCode{clp::RegexErrorEnum::Question}};
    std::error_code err_pip{clp::RegexErrorCode{clp::RegexErrorEnum::Pipe}};
    std::cerr << err_question.message() << "\n";
    std::cerr << err_question.category().name() << "\n";
    std::cerr << err_pip.message() << "\n";
    std::cerr << err_pip.category().name() << "\n";
    REQUIRE(err_question.category() == err_pip.category());
}
