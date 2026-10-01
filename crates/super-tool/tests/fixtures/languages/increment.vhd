library ieee;
use ieee.numeric_std.all;
use work.incrementer.all;

entity increment_supertests is
  port (value : in natural range 0 to 255);
end entity;

architecture supertests of increment_supertests is
begin
  --% supertest
  increment_never_decreases : process
  begin
    assert to_integer(saturating_increment(to_unsigned(value, 8))) >= value
      report "Increment decreased the value" severity failure;
    wait;
  end process;
end architecture;
