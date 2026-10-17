/-
The two textbook theorems, over all integers, checked by Lean.

The record verifies the finite core of each for itself: over the four
residues modulo 4, every square is 0 or 1, and no square is the sum of
two odd squares. Carrying that to every integer takes the reduction
lemmas below (reducing modulo m respects squaring and adding, and the
parity of an integer is the parity of its residue modulo 4) and the same
four-way case split. Lean does both here, from its core library alone;
the theorem names are the statements the record attests.
-/

theorem square_reduces (n m : Int) :
    n * n % m = (n % m) * (n % m) % m :=
  Int.mul_emod n n m

theorem sum_of_squares_reduces (a b m : Int) :
    (a * a + b * b) % m = ((a % m) * (a % m) + (b % m) * (b % m)) % m := by
  rw [Int.add_emod, Int.mul_emod a a m, Int.mul_emod b b m, ← Int.add_emod]

theorem parity_reduces_mod_four (n : Int) :
    n % 2 = (n % 4) % 2 :=
  (Int.emod_emod_of_dvd n (by decide : (2 : Int) ∣ 4)).symm

/-- Every integer square leaves remainder 0 or 1 modulo 4. -/
theorem every_square_is_zero_or_one_mod_four (n : Int) :
    n * n % 4 = 0 ∨ n * n % 4 = 1 := by
  rw [square_reduces]
  have cases : n % 4 = 0 ∨ n % 4 = 1 ∨ n % 4 = 2 ∨ n % 4 = 3 := by omega
  rcases cases with h | h | h | h <;> rw [h] <;> decide

/-- A Pythagorean triple cannot have both legs odd. -/
theorem no_pythagorean_triple_has_two_odd_legs (a b c : Int)
    (triple : a * a + b * b = c * c) : ¬ (a % 2 = 1 ∧ b % 2 = 1) := by
  intro ⟨odd_a, odd_b⟩
  have square := every_square_is_zero_or_one_mod_four c
  rw [← triple, sum_of_squares_reduces] at square
  rw [parity_reduces_mod_four] at odd_a odd_b
  have ra : a % 4 = 1 ∨ a % 4 = 3 := by omega
  have rb : b % 4 = 1 ∨ b % 4 = 3 := by omega
  rcases ra with ra | ra <;> rcases rb with rb | rb <;> rw [ra, rb] at square <;> revert square <;> decide
